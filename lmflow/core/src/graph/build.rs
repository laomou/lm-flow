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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::config::{GraphConfig, StatsLevel};
use crate::context::{Context, Options};
use crate::executor::{DelegatingExecutor, Executor, ThreadPool, DEFAULT_EXECUTOR_NAME};
use crate::kernel::{Contract, KernelInstance, PortTable};
use crate::runtime::{self, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

use super::{
    Activity, Edge, EdgeId, GraphInner, InputPolicy, InputQueueStats, Node, NodeSched, NodeStats,
    OnError, State,
};

// ---------------------------------------------------------------- 构建与校验

impl GraphInner {
    pub(super) fn build(cfg: GraphConfig) -> Result<Self> {
        let configured_stats = cfg.stats.unwrap_or_else(|| {
            cfg.stats_timing.map_or(StatsLevel::Basic, |enabled| {
                if enabled {
                    StatsLevel::Full
                } else {
                    StatsLevel::Basic
                }
            })
        });
        let stats_level = cfg.effective_stats_level();
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
        //
        // `executors` 里写的**一律是宿主自己的执行器**,必须有名字 —— 节点靠名字引用它。
        // 默认执行器则完全由引擎持有:按 CPU 核数开的线程池,恒在下标 0,YAML 无从干涉。
        // 因此 `default` 是**保留名**,写了报错;否则图里会同时出现两个 `default`。
        // 节点侧 `executor` 留空即归默认执行器,归一化到同一个名字。于是默认执行器和
        // 其它执行器完全同构 —— 有名字、可索引、可提交任务,派任务时无需为它开特例。
        let mut executors: Vec<Executor> = vec![Executor::Pool(default_thread_pool(stats_level))];
        for e in &cfg.executors {
            if e.name.is_empty() {
                return Err(Error::InvalidArg(
                    "executors entry must have a name; nodes select an executor by it".into(),
                ));
            }
            if e.name == DEFAULT_EXECUTOR_NAME {
                return Err(Error::InvalidArg(format!(
                    "executor name `{DEFAULT_EXECUTOR_NAME}` is reserved for the engine's implicit \
                     default executor (where nodes without an `executor` run) and cannot be declared. \
                     Pick another name; to control threads / affinity / priority, declare your own \
                     pool and point the nodes at it with `executor:`"
                )));
            }
            if executors.iter().any(|p| p.name() == e.name) {
                return Err(Error::InvalidArg(format!(
                    "executor `{}` defined more than once",
                    e.name
                )));
            }
            executors.push(build_executor(&e.name, e, stats_level)?);
        }
        let known: Vec<&str> = executors.iter().map(|p| p.name()).collect();
        for (idx, n) in cfg.nodes.iter().enumerate() {
            if !known.contains(&exec_name(&n.executor)) {
                return Err(Error::InvalidArg(format!(
                    "node `{}` references undefined executor `{}` (defined: [{}])",
                    node_label(n, idx),
                    n.executor,
                    known.join(", ")
                )));
            }
        }
        // 定义了却没人用的池只会白占线程,出声提醒。默认执行器豁免:它是引擎建的,
        // 宿主根本没声明过它,没人用不是宿主的错。
        for p in &executors {
            if p.name() == DEFAULT_EXECUTOR_NAME || p.is_delegating() {
                continue;
            }
            if !cfg.nodes.iter().any(|n| exec_name(&n.executor) == p.name()) {
                runtime::log_warn(&format!(
                    "executor `{}` is defined but not used by any node ({} threads will idle)",
                    p.name(),
                    p.num_threads()
                ));
            }
        }
        // ---- 校验 5b:节点与执行器的匹配(需要执行器已建好才判得了)----
        check_node_executor_fit(&cfg, &executors)?;

        // ---- 收集契约 + 静态类型检查 ----
        //
        // 所有节点的端口表此时都已确定,先一次性询问契约,才能沿每条边同时看到
        // producer.output_type 与 consumer.input_type。能静态证明不兼容的连接在建图期
        // 直接拒绝;任一侧为 ANY/NONE(0)则保留运行期逐包检查。
        let mut contracts = Vec::with_capacity(cfg.nodes.len());
        let mut kernels = Vec::with_capacity(cfg.nodes.len());
        for (idx, n) in cfg.nodes.iter().enumerate() {
            let name = node_label(n, idx);
            let (ins, outs) = node_port_tables[idx].clone();
            let mut contract = Contract::new(ins, outs);
            // 安全性:contract 是本栈帧上存活的对象,回调期间无人访问它。
            unsafe {
                KernelInstance::fill_contract(
                    &n.kernel,
                    &mut contract as *mut Contract as *mut c_void,
                )?
            };
            reject_reserved_contract_types(&name, &contract)?;
            contracts.push(contract);
            // 保持历史顺序:每个节点都是 get_contract 后立刻 create,而不是先询问完
            // 所有契约再统一创建。静态检查失败时这些实例随局部 Vec 正常析构。
            kernels.push(KernelInstance::create(&n.kernel)?);
        }
        check_edge_type_compatibility(&cfg, &edges, &contracts)?;

        // ---- 建节点 ----
        let shared = Arc::new(GraphShared::new(cfg.clone()));
        let mut nodes = Vec::new();
        let mut required: Vec<(String, String)> = Vec::new();
        for ((idx, n), (contract, kernel)) in cfg
            .nodes
            .iter()
            .enumerate()
            .zip(contracts.into_iter().zip(kernels))
        {
            let name = node_label(n, idx);
            let (ins, outs) = node_port_tables[idx].clone();

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

            // 上面已校验过名字存在(节点侧留空归一化到默认执行器),故必有下标。
            let executor = executors
                .iter()
                .position(|p| p.name() == exec_name(&n.executor))
                .expect("executor name was validated above");

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
                output_types: contract.output_types.clone(),
                kernel,
                ctxs,
                max_in_flight: mif,
                sched: Mutex::new(NodeSched::new(mif)),
                stats: NodeStats::default(),
                input_queues: (0..ins.len())
                    .map(|_| Mutex::new(VecDeque::new()))
                    .collect(),
                input_queue_capacity: ins
                    .names()
                    .iter()
                    .map(|port| {
                        n.input_queues
                            .ports
                            .get(port)
                            .and_then(|limits| limits.packets)
                            .unwrap_or(n.input_queues.packets)
                    })
                    .map(|capacity| (capacity != 0).then_some(capacity))
                    .collect(),
                input_queue_reserved: (0..ins.len()).map(|_| AtomicUsize::new(0)).collect(),
                input_queue_bytes: (0..ins.len()).map(|_| AtomicU64::new(0)).collect(),
                input_queue_stats: (0..ins.len()).map(|_| InputQueueStats::default()).collect(),
                input_closed: (0..ins.len()).map(|_| AtomicBool::new(false)).collect(),
                on_error: OnError::from_config(&n.on_error),
                min_period: if n.rate > 0.0 {
                    Some(std::time::Duration::from_secs_f64(1.0 / n.rate))
                } else {
                    None
                },
                last_fire: Mutex::new(None),
                input_is_back_edge: back_edge_mask[idx].clone(),
                source_done: AtomicBool::new(false),
                source_waiting: AtomicBool::new(false),
                source_wait_reason: AtomicUsize::new(0),
                source_wake_deadline_us: AtomicI64::new(0),
                source_yield_count: AtomicU64::new(0),
                source_wake_generation: AtomicU64::new(0),
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

        if configured_stats != StatsLevel::Full && shared.config.watchdog_ms > 0 {
            runtime::log_info(
                "stats is overridden to full because watchdog_ms > 0 (the watchdog needs per-call timing)",
            );
        } else if stats_level != StatsLevel::Full {
            runtime::log_info(
                "stats is not full: per-call timing, latency percentiles, CoW copies, and executor timing are disabled",
            );
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
            executors,
            in_flight: AtomicUsize::new(0),
            delegated_cursor: AtomicUsize::new(0),
            delegated_running: AtomicBool::new(false),
            activity: (Mutex::new(Activity::default()), Condvar::new()),
            wakeup_callback: Mutex::new(None),
            wakeup_pending: AtomicBool::new(false),
            wakeup_generation: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            blocked_flush_nodes: Mutex::new(BTreeSet::new()),
            side_packets: Mutex::new(BTreeMap::new()),
            required_side_packets: required,
            epoch: Instant::now(),
            run_started_us: AtomicI64::new(0),
            dot_intervals: Mutex::new(super::dot::DotIntervalBaselines::default()),
            stats_level,
        })
    }
}

/// 节点 `executor` 字段的归一化:空 = 引擎隐式的默认执行器。
///
/// 只作用于**节点侧**的引用 —— `executors` 条目必须有名字,且不得叫 `default`
/// (那个名字是引擎保留的,见 `DEFAULT_EXECUTOR_NAME`)。
fn exec_name(n: &str) -> &str {
    if n.is_empty() {
        DEFAULT_EXECUTOR_NAME
    } else {
        n
    }
}

/// 引擎隐式的默认执行器:按 CPU 核数开线程的线程池。
///
/// 不绑核、不设实时优先级,也不可配 —— 那些是场景相关的调优,宿主想控制就自己声明一个
/// 具名池(或具名 `DelegatingExecutor`),把节点用 `executor:` 指过去。
fn default_thread_pool(stats_level: StatsLevel) -> ThreadPool {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    ThreadPool::new_with_stats(DEFAULT_EXECUTOR_NAME, n, Vec::new(), 0, stats_level)
}

/// 按 `type` 建一个执行器。空 `type` 视作 `ThreadPoolExecutor`(历史默认)。
///
/// `DelegatingExecutor` 不拥有线程,故 `num_threads`/`affinity`/`priority` 对它没有意义 ——
/// 写了就报错,不静默忽略(与「未知值明确拒掉」同规矩)。
fn build_executor(
    name: &str,
    e: &crate::config::ExecutorConfig,
    stats_level: StatsLevel,
) -> Result<Executor> {
    if e.r#type == "DelegatingExecutor" {
        for (field, set) in [
            ("num_threads", e.num_threads != 0),
            ("affinity", !e.affinity.is_empty()),
            ("priority", e.priority != 0),
        ] {
            if set {
                return Err(Error::InvalidArg(format!(
                    "executor `{name}`: DelegatingExecutor owns no threads, so `{field}` is \
                     meaningless -- it hands ready nodes back to the host thread. Drop the field, \
                     or use type: \"ThreadPoolExecutor\" if you wanted engine-owned threads"
                )));
            }
        }
        return Ok(Executor::Delegating(DelegatingExecutor::new_with_stats(
            name,
            stats_level,
        )));
    }
    Ok(Executor::Pool(ThreadPool::new_with_stats(
        name,
        e.num_threads,
        e.affinity.clone(),
        e.priority,
        stats_level,
    )))
}

/// 节点与它所属执行器是否匹配。**必须在执行器建好之后**判 —— 这两条规则问的都是
/// 「解析出来的执行器长什么样」,光看 YAML 里的名字答不上来。
fn check_node_executor_fit(cfg: &GraphConfig, executors: &[Executor]) -> Result<()> {
    for (idx, n) in cfg.nodes.iter().enumerate() {
        let who = node_label(n, idx);
        let ei = executors
            .iter()
            .position(|p| p.name() == exec_name(&n.executor))
            .expect("executor name was validated above");
        let ex = &executors[ei];

        // max_in_flight > 1 只有落在多线程池上才真有并行度。委托执行器(0 线程)
        // 与单线程池都恒为 1,宁可报错也不让宿主误以为开了并行。
        if n.max_in_flight > 1 && ex.num_threads() < 2 {
            return Err(Error::InvalidArg(format!(
                "node `{who}`: max_in_flight={} needs an executor with more than one thread, \
                 but `{}` provides {} -- there would be no parallelism",
                n.max_in_flight,
                ex.name(),
                ex.num_threads()
            )));
        }

        if n.input_ports.is_empty() {
            // 源节点跑在委托执行器上会独占宿主线程、拖垮全图 —— 宿主还得回来
            // 抽取别的委托任务,而它正卡在源的 process 里。
            if ex.is_delegating() {
                return Err(Error::InvalidArg(format!(
                    "node `{who}`: a source node (no input ports) cannot run on the delegating \
                     executor `{}` -- its process typically blocks waiting for the next item, \
                     which would monopolise the host thread and stall the whole graph. \
                     Give it a ThreadPoolExecutor",
                    ex.name()
                )));
            }
        }
    }
    Ok(())
}

fn reject_reserved_contract_types(name: &str, contract: &Contract) -> Result<()> {
    // `HOST_OBJECT`(7)是预留未启用类型(ADR #26),契约声明必须建图期失败。
    for (which, types) in [
        ("input", &contract.input_types),
        ("output", &contract.output_types),
    ] {
        if let Some(i) = types
            .iter()
            .position(|&t| t == crate::packet::type_id::HOST_OBJECT)
        {
            return Err(Error::InvalidArg(format!(
                "node `{name}`: {which} port {i} declares LMFLOW_TYPE_HOST_OBJECT, \
                 which is reserved and not enabled (see ADR #26). Host-language native \
                 objects (e.g. PyObject) would create a second type system invisible to \
                 the YAML graph, and their refcount can drop on an engine worker thread \
                 where releasing them needs the GIL. Use LMFLOW_TYPE_BUFFER for numeric \
                 collections, or LMFLOW_TYPE_STR carrying JSON for arbitrary metadata"
            )));
        }
    }
    Ok(())
}

fn check_edge_type_compatibility(
    cfg: &GraphConfig,
    edges: &[Edge],
    contracts: &[Contract],
) -> Result<()> {
    for edge in edges {
        let Some(producer) = edge.producer else {
            continue; // 图输入的实际类型由宿主逐包决定,只能运行期检查。
        };
        let producer_port = contracts[producer]
            .outputs
            .index_by_name(&edge.name)
            .expect("edge producer output must exist");
        let produced = contracts[producer].output_types[producer_port];
        if produced == crate::packet::type_id::NONE {
            continue; // producer 声明 ANY:无法静态证明,保留运行期检查。
        }
        for &(consumer, consumer_port) in &edge.consumers {
            let wanted = contracts[consumer].input_types[consumer_port];
            if wanted == crate::packet::type_id::NONE || wanted == produced {
                continue;
            }
            return Err(Error::InvalidArg(format!(
                "type mismatch on edge `{}`: node `{}` output port `{}` declares {}, \
                 but node `{}` input port `{}` declares {}",
                edge.name,
                node_label(&cfg.nodes[producer], producer),
                contracts[producer]
                    .outputs
                    .name(producer_port)
                    .unwrap_or("?"),
                crate::packet::type_name(produced),
                node_label(&cfg.nodes[consumer], consumer),
                contracts[consumer]
                    .inputs
                    .name(consumer_port)
                    .unwrap_or("?"),
                crate::packet::type_name(wanted),
            )));
        }
    }
    Ok(())
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
