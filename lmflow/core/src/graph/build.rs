//! 建图与建图期校验:把 `GraphConfig` 变成可执行的 [`GraphInner`]。
//!
//! 与 [`super::dot`] / [`super::introspect`] 同理,单独成模块是为了让 `mod.rs` 只剩
//! **运行期并发核心**(调度 / 派发 / 终止 / 锁序)。这里跑的都是**一次性**逻辑:
//! 端口解析、契约询问、拓扑校验 —— 出错就建图失败,不涉及任何并发。
//! 本模块是 `graph` 的子模块,故可访问私有字段。
//!
//! 注意:**逐包**的运行期校验(`check_input_types` / `check_input_monotonic`)留在
//! `mod.rs`,它们在派发路径上,属于并发核心的一部分。

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::config::GraphConfig;
use crate::context::{Context, Options};
use crate::executor::ThreadPool;
use crate::kernel::{Contract, KernelInstance, PortTable};
use crate::runtime::{self, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

use super::{Edge, EdgeId, GraphInner, InputPolicy, Node, NodeSched, NodeStats, State};

// ---------------------------------------------------------------- 构建与校验

impl GraphInner {
    pub(super) fn build(cfg: GraphConfig) -> Result<Self> {
        let mut edges: Vec<Edge> = Vec::new();
        let mut edge_by_name: BTreeMap<String, EdgeId> = BTreeMap::new();

        let mut get_or_create = |name: &str, edges: &mut Vec<Edge>| -> EdgeId {
            if let Some(&id) = edge_by_name.get(name) {
                return id;
            }
            let id = edges.len();
            edges.push(Edge::new(name.to_string()));
            edge_by_name.insert(name.to_string(), id);
            id
        };

        // ---- 图输入口 ----
        let mut graph_inputs = Vec::new();
        let mut input_by_name = BTreeMap::new();
        for decl in &cfg.input_ports {
            let spec = crate::config::parse_port_spec(decl)?;
            if input_by_name.contains_key(&spec.name) {
                return Err(Error::InvalidArg(format!(
                    "graph input port `{}` declared more than once",
                    spec.name
                )));
            }
            let id = get_or_create(&spec.name, &mut edges);
            edges[id].is_graph_input = true;
            graph_inputs.push(id);
            input_by_name.insert(spec.name, id);
        }

        // ---- 节点的输出口:确定生产者(校验 2、3)----
        let mut node_port_tables: Vec<(Arc<PortTable>, Arc<PortTable>)> = Vec::new();
        for (idx, n) in cfg.nodes.iter().enumerate() {
            let who = node_label(n, idx);
            let ins = Arc::new(PortTable::build(
                &n.input_ports,
                &format!("node `{who}` input ports"),
            )?);
            let outs = Arc::new(PortTable::build(
                &n.output_ports,
                &format!("node `{who}` output ports"),
            )?);
            for name in outs.names() {
                let id = get_or_create(name, &mut edges);
                if edges[id].is_graph_input {
                    return Err(Error::InvalidArg(format!(
                        "port name `{name}` is both a graph input port and node `{who}`'s output port -- name conflict"
                    )));
                }
                if let Some(prev) = edges[id].producer {
                    return Err(Error::InvalidArg(format!(
                        "port `{name}` has multiple producers: node `{}` and `{who}`",
                        node_label(&cfg.nodes[prev], prev)
                    )));
                }
                edges[id].producer = Some(idx);
            }
            node_port_tables.push((ins, outs));
        }

        // ---- 节点的输入口:连接消费者(校验 1)----
        for (idx, n) in cfg.nodes.iter().enumerate() {
            let who = node_label(n, idx);
            let ins = node_port_tables[idx].0.clone();
            // 0 输入 = 源节点(生成型算子):内核自产,无需连消费边。执行器必需性在
            // config.check_supported 校验;源的输出边已在生产者环节连好。
            for (port, name) in ins.names().iter().enumerate() {
                let id = *edge_by_name.get(name).ok_or_else(|| {
                    Error::InvalidArg(format!(
                        "node `{who}` input port `{name}` has no producer: neither a graph input port nor produced by any node"
                    ))
                })?;
                if edges[id].producer.is_none() && !edges[id].is_graph_input {
                    return Err(Error::InvalidArg(format!(
                        "port `{name}` has no producer (node `{who}` consumes it)"
                    )));
                }
                edges[id].consumers.push((idx, port));
            }
        }

        // ---- 图输出口 ----
        let mut graph_outputs = Vec::new();
        let mut output_by_name = BTreeMap::new();
        for decl in &cfg.output_ports {
            let spec = crate::config::parse_port_spec(decl)?;
            let id = *edge_by_name.get(&spec.name).ok_or_else(|| {
                Error::InvalidArg(format!(
                    "graph output port `{}`: no node produces it",
                    spec.name
                ))
            })?;
            edges[id].is_graph_output = true;
            graph_outputs.push(id);
            output_by_name.insert(spec.name, id);
        }

        // ---- 无人消费的端口:静默丢包是最难查的故障,至少要出声 ----
        for e in &edges {
            if !e.consumers.is_empty() || e.is_graph_output {
                continue;
            }
            if e.is_graph_input {
                runtime::log_warn(&format!(
                    "graph input port `{}` is consumed by no node -- packets sent in will be dropped",
                    e.name
                ));
            } else if let Some(p) = e.producer {
                runtime::log_warn(&format!(
                    "node `{}` output port `{}` has no downstream consumer and is not a graph output port -- output will be dropped",
                    node_label(&cfg.nodes[p], p),
                    e.name
                ));
            }
        }

        // 每节点每输入口是否为 back-edge(按口名匹配 config.back_edges)。反馈寄存器口不参与
        // 拓扑成环判定 / 就绪 / 终止 / 对齐。
        let back_edge_mask: Vec<Vec<bool>> = cfg
            .nodes
            .iter()
            .enumerate()
            .map(|(idx, nc)| {
                node_port_tables[idx]
                    .0
                    .names()
                    .iter()
                    .map(|name| nc.back_edges.contains(name))
                    .collect()
            })
            .collect();

        // ---- 校验 4:成环(back-edge 打断的环放行)----
        check_acyclic(&cfg, &edges, &back_edge_mask)?;

        // ---- 校验 5 + 建执行器 ----
        let mut executors: Vec<ThreadPool> = Vec::new();
        for e in &cfg.executors {
            if e.name.is_empty() {
                return Err(Error::InvalidArg(
                    "executors entry must have a name; nodes select a thread pool by it".into(),
                ));
            }
            if executors.iter().any(|p| p.name() == e.name) {
                return Err(Error::InvalidArg(format!(
                    "executor `{}` defined more than once",
                    e.name
                )));
            }
            executors.push(ThreadPool::new(
                &e.name,
                e.num_threads,
                e.affinity.clone(),
                e.priority,
            ));
        }
        let known: Vec<&str> = executors.iter().map(|p| p.name()).collect();
        for (idx, n) in cfg.nodes.iter().enumerate() {
            if !n.executor.is_empty() && !known.contains(&n.executor.as_str()) {
                return Err(Error::InvalidArg(format!(
                    "node `{}` references undefined executor `{}` (defined: [{}])",
                    node_label(n, idx),
                    n.executor,
                    known.join(", ")
                )));
            }
        }
        // 定义了却没人用的池只会白占线程,出声提醒
        for p in &executors {
            if !cfg.nodes.iter().any(|n| n.executor == p.name()) {
                runtime::log_warn(&format!(
                    "executor `{}` is defined but not used by any node ({} threads will idle)",
                    p.name(),
                    p.num_threads()
                ));
            }
        }

        // ---- 建节点 ----
        let shared = Arc::new(GraphShared::new(cfg.clone()));
        let mut nodes = Vec::new();
        let mut required: Vec<(String, String)> = Vec::new();
        for (idx, n) in cfg.nodes.iter().enumerate() {
            let name = node_label(n, idx);
            let (ins, outs) = node_port_tables[idx].clone();

            // 契约:端口数与名字已知,算子只补类型 + 声明必需 side packet
            let mut contract = Contract::new(ins.clone(), outs.clone());
            // 安全性:contract 是本栈帧上存活的对象,回调期间无人访问它
            unsafe {
                KernelInstance::fill_contract(
                    &n.kernel,
                    &mut contract as *mut Contract as *mut c_void,
                )?
            };
            let kernel = KernelInstance::create(&n.kernel)?;

            let input_edges: Vec<EdgeId> = ins.names().iter().map(|x| edge_by_name[x]).collect();
            let output_edges: Vec<EdgeId> = outs.names().iter().map(|x| edge_by_name[x]).collect();

            // 0 视作 1。max_in_flight 个并行调用各需一个 context 槽。
            let mif = n.max_in_flight.max(1);
            let options = Arc::new(Options::new(n.options.clone()));
            let make_ctx = || {
                Context::new(
                    name.clone(),
                    n.kernel.clone(),
                    ins.clone(),
                    outs.clone(),
                    options.clone(),
                    Arc::new(BTreeMap::new()), // start 时替换为真实 side packets
                    shared.clone(),
                )
            };
            let ctxs: Vec<UnsafeCell<Context>> =
                (0..mif).map(|_| UnsafeCell::new(make_ctx())).collect();

            let executor = if n.executor.is_empty() {
                None
            } else {
                executors.iter().position(|p| p.name() == n.executor)
            };

            nodes.push(Node {
                name,
                kernel_name: n.kernel.clone(),
                inputs: input_edges,
                outputs: output_edges,
                in_ports: ins.clone(),
                out_ports: outs,
                executor,
                policy: InputPolicy::from_config(&n.input_policy, &ins)?,
                input_types: contract.input_types.clone(),
                kernel,
                ctxs,
                max_in_flight: mif,
                sched: Mutex::new(NodeSched::new(mif)),
                stats: NodeStats::default(),
                input_queues: (0..ins.len())
                    .map(|_| Mutex::new(VecDeque::new()))
                    .collect(),
                input_closed: (0..ins.len()).map(|_| AtomicBool::new(false)).collect(),
                input_is_back_edge: back_edge_mask[idx].clone(),
                source_done: AtomicBool::new(false),
                input_bounds: (0..ins.len())
                    .map(|_| Mutex::new(Timestamp::pre_stream()))
                    .collect(),
            });
            // 记录该算子声明的必需 side packet,start 时校验
            for name in &contract.required_side_packets {
                required.push((
                    name.clone(),
                    nodes.last().expect("just inserted").name.clone(),
                ));
            }
        }

        Ok(Self {
            shared,
            nodes,
            edges,
            graph_inputs,
            graph_outputs,
            input_by_name,
            output_by_name,
            edge_by_name,
            state: Mutex::new(State::Initialized),
            main_queue: Mutex::new(VecDeque::new()),
            executors,
            in_flight: AtomicUsize::new(0),
            activity: (Mutex::new(0), Condvar::new()),
            paused: AtomicBool::new(false),
            side_packets: Mutex::new(BTreeMap::new()),
            required_side_packets: required,
            epoch: Instant::now(),
        })
    }
}

fn node_label(n: &crate::config::NodeConfig, idx: usize) -> String {
    if n.name.is_empty() {
        format!("{}#{}", n.kernel, idx)
    } else {
        n.name.clone()
    }
}

/// 拓扑成环检测。back-edge(反馈寄存器口)标记的消费边**不算进拓扑** —— 它正是用来打断环的;
/// 未被 back-edge 打断的环仍报错(会死锁/无法终止)。`back_edge_mask[node][port]` = 该口是否 back-edge。
fn check_acyclic(cfg: &GraphConfig, edges: &[Edge], back_edge_mask: &[Vec<bool>]) -> Result<()> {
    let n = cfg.nodes.len();
    // 邻接:生产者 → 消费者
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in edges {
        if let Some(p) = e.producer {
            for &(c, port) in &e.consumers {
                if back_edge_mask[c][port] {
                    continue; // back-edge:反馈方向不计入拓扑,故不成环
                }
                adj[p].push(c);
            }
        }
    }

    /// 0 = 未访问, 1 = 在当前 DFS 栈上, 2 = 已完成
    const UNVISITED: u8 = 0;
    const ON_STACK: u8 = 1;
    const DONE: u8 = 2;

    let mut mark = vec![UNVISITED; n];
    // 显式栈而非递归:深图不应爆栈
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for start in 0..n {
        if mark[start] != UNVISITED {
            continue;
        }
        mark[start] = ON_STACK;
        stack.push((start, 0));
        while let Some(&mut (node, ref mut cursor)) = stack.last_mut() {
            if *cursor < adj[node].len() {
                let next = adj[node][*cursor];
                *cursor += 1;
                match mark[next] {
                    ON_STACK => {
                        return Err(Error::InvalidArg(format!(
                            "topology cycle: node `{}` -> ... -> `{}` -- break it by marking a feedback input with `back_edges` (an unbroken cycle would never terminate)",
                            node_label(&cfg.nodes[next], next),
                            node_label(&cfg.nodes[node], node)
                        )));
                    }
                    UNVISITED => {
                        mark[next] = ON_STACK;
                        stack.push((next, 0));
                    }
                    _ => {}
                }
            } else {
                mark[node] = DONE;
                stack.pop();
            }
        }
    }
    Ok(())
}
