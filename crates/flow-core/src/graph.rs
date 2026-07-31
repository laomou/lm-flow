//! 图:拓扑、校验、执行、关流。
//!
//! 结构上用**索引 arena**:实体存在 `Vec` 里,互相引用用 `usize` id
//! (docs/design.md §6.1),避免自引用指针图。
//!
//! 队列**属于消费者的输入口**而不是边:一条边可以有多个消费者,各自必须收到每个包,
//! 共用一个队列会互相抢包。因此边只保存拓扑与关闭状态,包按消费者数分发(仅克隆引用)。

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::GraphConfig;
use crate::context::{Context, Options};
use crate::kernel::{Contract, KernelInstance, PortTable};
use crate::packet::Packet;
use crate::runtime::{self, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

pub type NodeId = usize;
pub type EdgeId = usize;

// ---------------------------------------------------------------- 状态机

/// 与 `FlowGraphState` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Created = 0,
    Initialized = 1,
    Running = 2,
    Draining = 3,
    Terminated = 4,
}

// ---------------------------------------------------------------- 边

pub struct Edge {
    pub name: String,
    pub producer: Option<NodeId>,
    /// (消费者节点, 它的第几个输入口)
    pub consumers: Vec<(NodeId, usize)>,
    pub is_graph_input: bool,
    pub is_graph_output: bool,
    closed: AtomicBool,
    dropped: AtomicU64,
    /// 该边上最近一次投递的时间戳。**必须独立记录**,不能拿「队列里还剩的包」当参照 ——
    /// 队列一排空参照就消失了,回退的时间戳就能混进来。
    last_sent: Mutex<Timestamp>,
    pollers: Mutex<Vec<Arc<PollerInner>>>,
    observers: Mutex<Vec<Observer>>,
}

impl Edge {
    fn new(name: String) -> Self {
        Self {
            name,
            producer: None,
            consumers: Vec::new(),
            is_graph_input: false,
            is_graph_output: false,
            closed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            last_sent: Mutex::new(Timestamp::unset()),
            pollers: Mutex::new(Vec::new()),
            observers: Mutex::new(Vec::new()),
        }
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

struct Observer {
    cb: unsafe extern "C" fn(*mut c_void, crate::ffi::FlowPacket),
    user: *mut c_void,
}
// 安全性:user 是宿主的不透明指针,引擎只原样回传。
unsafe impl Send for Observer {}

// ---------------------------------------------------------------- poller

pub struct PollerInner {
    edge: EdgeId,
    queue: Mutex<VecDeque<Packet>>,
    closed: AtomicBool,
}

impl PollerInner {
    fn push(&self, p: Packet) {
        self.queue.lock().expect("poller 锁中毒").push_back(p);
    }
    fn pop(&self) -> Option<Packet> {
        self.queue.lock().expect("poller 锁中毒").pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.lock().expect("poller 锁中毒").is_empty()
    }
}

/// 拉模式输出句柄。
pub struct Poller {
    graph: Arc<GraphInner>,
    inner: Arc<PollerInner>,
}

impl Poller {
    /// 取下一个包。本版本执行器是宿主主线程,故等待期间会**抽取并执行主线程任务**
    /// (见 docs/design.md §7.9)。图已结束且队空时返回 `None`。
    pub fn next(&self) -> Option<Packet> {
        loop {
            if let Some(p) = self.inner.pop() {
                return Some(p);
            }
            if self.inner.closed.load(Ordering::SeqCst) || self.graph.shared.has_error() {
                return None;
            }
            // 没有现成数据:推进一步引擎;推不动说明确实没有更多输出
            if !self.graph.pump_step() {
                return self.inner.pop();
            }
        }
    }

    /// 非阻塞:仅看现有队列。
    pub fn try_next(&self) -> Option<Packet> {
        self.inner.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

// ---------------------------------------------------------------- 节点

#[derive(Debug, Default)]
struct NodeSched {
    opened: bool,
    closed: bool,
    /// 正在执行算子回调 —— 独占令牌(docs/design.md §7.0 R3)
    running: bool,
    /// 运行期间又来了包,跑完必须重扫,否则丢唤醒
    rescan: bool,
}

#[derive(Debug, Default)]
struct NodeStats {
    processed: u64,
    errors: u64,
    total_us: i64,
    max_us: i64,
}

pub struct Node {
    pub name: String,
    pub kernel_name: String,
    pub inputs: Vec<EdgeId>,
    pub outputs: Vec<EdgeId>,
    pub in_ports: Arc<PortTable>,
    pub out_ports: Arc<PortTable>,
    input_types: Vec<u64>,
    kernel: KernelInstance,
    /// 用 UnsafeCell 而非 Mutex:算子回调期间引擎必须交出一个 `*mut Context` 给 C 侧,
    /// 若同时持有 Mutex guard 的 `&mut`,回调里从裸指针再造 `&mut` 就构成别名 UB。
    /// 独占性由「节点独占令牌」保证(docs/design.md §7.0 R3)。
    ctx: Box<UnsafeCell<Context>>,
    sched: Mutex<NodeSched>,
    stats: Mutex<NodeStats>,
    /// 每个输入口一条独立队列(见模块头注释)
    input_queues: Vec<Mutex<VecDeque<Packet>>>,
    input_closed: Vec<AtomicBool>,
    /// 正在执行算子回调的起始时刻 —— 让「卡死」可定位
    running_since: Mutex<Option<Instant>>,
}

// 安全性:Node 内的 UnsafeCell<Context> 只在持有该节点独占令牌时被访问,
// 令牌由 sched.running 在互斥下置位保证(docs/design.md §7.0 R3)。
unsafe impl Sync for Node {}

impl Node {
    /// # Safety
    /// 调用者必须持有本节点的独占令牌(`sched.running == true`),或处于
    /// 尚未开始调度的阶段(build/start)。
    #[allow(clippy::mut_from_ref)]
    unsafe fn ctx(&self) -> &mut Context {
        &mut *self.ctx.get()
    }

    fn queue_len(&self, port: usize) -> usize {
        self.input_queues[port].lock().expect("队列锁中毒").len()
    }
    fn all_inputs_have_data(&self) -> bool {
        (0..self.input_queues.len()).all(|i| self.queue_len(i) > 0)
    }
    fn all_inputs_closed_and_drained(&self) -> bool {
        (0..self.input_queues.len())
            .all(|i| self.input_closed[i].load(Ordering::SeqCst) && self.queue_len(i) == 0)
    }
}

// ---------------------------------------------------------------- 图

pub struct GraphInner {
    pub shared: Arc<GraphShared>,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    graph_inputs: Vec<EdgeId>,
    graph_outputs: Vec<EdgeId>,
    input_by_name: BTreeMap<String, EdgeId>,
    output_by_name: BTreeMap<String, EdgeId>,
    edge_by_name: BTreeMap<String, EdgeId>,
    state: Mutex<State>,
    /// 主线程执行器的任务队列(默认执行器,见 ADR #16)
    main_queue: Mutex<VecDeque<NodeId>>,
    side_packets: Mutex<BTreeMap<String, Packet>>,
    /// 各算子在 GetContract 里声明的必需 side packet:(名字, 声明它的节点)
    required_side_packets: Vec<(String, String)>,
}

/// 图句柄。
pub struct Graph {
    inner: Arc<GraphInner>,
}

impl Graph {
    pub fn from_yaml(text: &str) -> Result<Self> {
        let cfg = GraphConfig::from_yaml(text)?;
        Self::from_config(cfg)
    }

    pub fn from_yaml_file(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::InvalidArg(format!("读取 `{path}` 失败: {e}")))?;
        Self::from_yaml(&text)
    }

    pub fn from_config(cfg: GraphConfig) -> Result<Self> {
        let inner = GraphInner::build(cfg)?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn state(&self) -> State {
        *self.inner.state.lock().expect("状态锁中毒")
    }

    pub fn inner(&self) -> &Arc<GraphInner> {
        &self.inner
    }

    /// 注入常量输入。必须在 `start` 之前。
    pub fn set_side_packet(&self, name: &str, pkt: Packet) -> Result<()> {
        if self.state() != State::Initialized {
            return Err(Error::State("side packet 必须在 start 之前注入".into()));
        }
        self.inner
            .side_packets
            .lock()
            .expect("side packet 锁中毒")
            .insert(name.to_string(), pkt);
        Ok(())
    }

    pub fn add_poller(&self, port: &str) -> Result<Poller> {
        let st = self.state();
        if st != State::Initialized {
            return Err(Error::State(format!(
                "add_poller 必须在 start 之前调用(当前状态 {st:?})"
            )));
        }
        let edge = *self
            .inner
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("图输出口 `{port}` 不存在")))?;
        let inner = Arc::new(PollerInner {
            edge,
            queue: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
        });
        self.inner.edges[edge]
            .pollers
            .lock()
            .expect("poller 列表锁中毒")
            .push(inner.clone());
        Ok(Poller {
            graph: self.inner.clone(),
            inner,
        })
    }

    pub fn start(&self) -> Result<()> {
        self.inner.start()
    }

    pub fn input(&self, port: &str) -> Result<Input> {
        let edge = *self
            .inner
            .input_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("图输入口 `{port}` 不存在")))?;
        Ok(Input {
            graph: self.inner.clone(),
            edge,
        })
    }

    pub fn close_input(&self, port: &str) -> Result<()> {
        let edge = *self
            .inner
            .input_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("图输入口 `{port}` 不存在")))?;
        self.inner.close_edge(edge);
        self.inner.set_state_draining_if_all_inputs_closed();
        Ok(())
    }

    pub fn close_all_inputs(&self) {
        for &e in &self.inner.graph_inputs {
            self.inner.close_edge(e);
        }
        self.inner.set_state_draining_if_all_inputs_closed();
    }

    pub fn cancel(&self) {
        self.inner.shared.cancel();
    }

    pub fn wait_done(&self) -> Result<()> {
        self.inner.wait_done()
    }

    pub fn wait_until_idle(&self) -> Result<()> {
        while self.inner.pump_step() {}
        match self.inner.shared.first_error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn node_count(&self) -> usize {
        self.inner.nodes.len()
    }
    pub fn node_name(&self, i: usize) -> Option<&str> {
        self.inner.nodes.get(i).map(|n| n.name.as_str())
    }
    pub fn input_port_names(&self) -> Vec<&str> {
        self.inner
            .graph_inputs
            .iter()
            .map(|&e| self.inner.edges[e].name.as_str())
            .collect()
    }
    pub fn output_port_names(&self) -> Vec<&str> {
        self.inner
            .graph_outputs
            .iter()
            .map(|&e| self.inner.edges[e].name.as_str())
            .collect()
    }

    /// 指定边的积压包数(该边所有消费者输入队列之和)。
    pub fn queue_depth(&self, port: &str) -> Option<usize> {
        let e = *self.inner.edge_by_name.get(port)?;
        Some(self.inner.queue_depth(e))
    }

    pub fn dropped_count(&self, port: &str) -> Option<u64> {
        let e = *self.inner.edge_by_name.get(port)?;
        Some(self.inner.edges[e].dropped_count())
    }

    pub fn dump(&self) -> String {
        self.inner.dump()
    }

    pub fn node_stats(&self, i: usize) -> Option<NodeStatsSnapshot> {
        self.inner.node_stats(i)
    }
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Graph{{state:{:?}, nodes:{}, inputs:{:?}, outputs:{:?}}}",
            self.state(),
            self.inner.nodes.len(),
            self.input_port_names(),
            self.output_port_names()
        )
    }
}

/// 图输入口句柄(热路径免按名字查表)。
pub struct Input {
    graph: Arc<GraphInner>,
    edge: EdgeId,
}

impl Input {
    pub fn send(&self, pkt: Packet) -> Result<()> {
        self.graph.send(self.edge, pkt, true)
    }
    pub fn try_send(&self, pkt: Packet) -> Result<()> {
        self.graph.send(self.edge, pkt, false)
    }
    pub fn close(&self) {
        self.graph.close_edge(self.edge);
        self.graph.set_state_draining_if_all_inputs_closed();
    }
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Input{{port:`{}`}}", self.graph.edges[self.edge].name)
    }
}

impl std::fmt::Debug for Poller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Poller{{port:`{}`, pending:{}}}",
            self.graph.edges[self.inner.edge].name,
            self.inner.queue.lock().map(|q| q.len()).unwrap_or(0)
        )
    }
}

/// `FlowNodeStats` 的 Rust 侧快照。
#[derive(Debug, Clone)]
pub struct NodeStatsSnapshot {
    pub node_name: String,
    pub kernel_name: String,
    pub running: bool,
    pub running_for_us: i64,
    pub processed: u64,
    pub errors: u64,
    pub total_process_us: i64,
    pub max_process_us: i64,
    pub queued: usize,
}

// ---------------------------------------------------------------- 构建与校验

impl GraphInner {
    fn build(cfg: GraphConfig) -> Result<Self> {
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
                    "图输入口 `{}` 重复声明",
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
                &format!("节点 `{who}` 的输入口"),
            )?);
            let outs = Arc::new(PortTable::build(
                &n.output_ports,
                &format!("节点 `{who}` 的输出口"),
            )?);
            for name in outs.names() {
                let id = get_or_create(name, &mut edges);
                if edges[id].is_graph_input {
                    return Err(Error::InvalidArg(format!(
                        "端口名 `{name}` 既是图输入口又是节点 `{who}` 的输出口 —— 名字冲突"
                    )));
                }
                if let Some(prev) = edges[id].producer {
                    return Err(Error::InvalidArg(format!(
                        "端口 `{name}` 有多个生产者:节点 `{}` 与 `{who}`",
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
            if ins.is_empty() {
                // 零输入节点在 B 阶段的就绪规则下恒就绪,会被无限调度成自旋
                return Err(Error::Unsupported(format!(
                    "节点 `{who}` 没有输入口 —— source 节点尚未支持(见 docs/design.md §7.4)"
                )));
            }
            for (port, name) in ins.names().iter().enumerate() {
                let id = *edge_by_name.get(name).ok_or_else(|| {
                    Error::InvalidArg(format!(
                        "节点 `{who}` 的输入口 `{name}` 找不到生产者:既非图输入口,也无节点输出它"
                    ))
                })?;
                if edges[id].producer.is_none() && !edges[id].is_graph_input {
                    return Err(Error::InvalidArg(format!(
                        "端口 `{name}` 无生产者(节点 `{who}` 在消费它)"
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
                Error::InvalidArg(format!("图输出口 `{}` 没有任何节点产出它", spec.name))
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
                    "图输入口 `{}` 没有任何节点消费它 —— 送进来的包会被直接丢弃",
                    e.name
                ));
            } else if let Some(p) = e.producer {
                runtime::log_warn(&format!(
                    "节点 `{}` 的输出口 `{}` 既无下游消费者、也不是图输出口 —— 产出会被直接丢弃",
                    node_label(&cfg.nodes[p], p),
                    e.name
                ));
            }
        }

        // ---- 校验 4:成环 ----
        check_acyclic(&cfg, &edges)?;

        // ---- 校验 5:executor 名 ----
        let known: Vec<&str> = cfg.executors.iter().map(|e| e.name.as_str()).collect();
        for (idx, n) in cfg.nodes.iter().enumerate() {
            if !n.executor.is_empty() && !known.contains(&n.executor.as_str()) {
                return Err(Error::InvalidArg(format!(
                    "节点 `{}` 引用了未定义的 executor `{}`(已定义: [{}])",
                    node_label(n, idx),
                    n.executor,
                    known.join(", ")
                )));
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

            let ctx = Context::new(
                name.clone(),
                n.kernel.clone(),
                ins.clone(),
                outs.clone(),
                Arc::new(Options::new(n.options.clone())),
                Arc::new(BTreeMap::new()), // start 时替换为真实 side packets
                shared.clone(),
            );

            nodes.push(Node {
                name,
                kernel_name: n.kernel.clone(),
                inputs: input_edges,
                outputs: output_edges,
                in_ports: ins.clone(),
                out_ports: outs,
                input_types: contract.input_types.clone(),
                kernel,
                ctx: Box::new(UnsafeCell::new(ctx)),
                sched: Mutex::new(NodeSched::default()),
                stats: Mutex::new(NodeStats::default()),
                input_queues: (0..ins.len())
                    .map(|_| Mutex::new(VecDeque::new()))
                    .collect(),
                input_closed: (0..ins.len()).map(|_| AtomicBool::new(false)).collect(),
                running_since: Mutex::new(None),
            });
            // 记录该算子声明的必需 side packet,start 时校验
            for name in &contract.required_side_packets {
                required.push((name.clone(), nodes.last().expect("刚插入").name.clone()));
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
            side_packets: Mutex::new(BTreeMap::new()),
            required_side_packets: required,
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

/// 拓扑成环检测。本版本不支持 back-edge,成环会直接死锁,故 init 阶段就拒绝。
fn check_acyclic(cfg: &GraphConfig, edges: &[Edge]) -> Result<()> {
    let n = cfg.nodes.len();
    // 邻接:生产者 → 消费者
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in edges {
        if let Some(p) = e.producer {
            for &(c, _) in &e.consumers {
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
                            "拓扑成环:节点 `{}` → … → `{}`(本版本不支持 back-edge,成环会死锁)",
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

// ---------------------------------------------------------------- 执行

impl GraphInner {
    fn state(&self) -> State {
        *self.state.lock().expect("状态锁中毒")
    }
    fn set_state(&self, s: State) {
        *self.state.lock().expect("状态锁中毒") = s;
    }

    fn start(&self) -> Result<()> {
        let st = self.state();
        if st != State::Initialized {
            return Err(Error::State(format!(
                "start 只能在 Initialized 调用(当前 {st:?})"
            )));
        }

        // 校验算子声明的必需 side packet
        let provided = self.side_packets.lock().expect("side packet 锁中毒");
        for (need, who) in &self.required_side_packets {
            if !provided.contains_key(need) {
                return Err(Error::InvalidArg(format!(
                    "缺少必需的 side packet `{need}` —— 节点 `{who}` 在 GetContract 中声明了它;\
                     请在 start 之前用 set_side_packet 注入"
                )));
            }
        }
        let sp = Arc::new(provided.clone());
        drop(provided);

        // 把 side packets 灌进各节点上下文,然后 open
        for (i, node) in self.nodes.iter().enumerate() {
            {
                let ctx = unsafe { node.ctx() };
                ctx.side_packets = sp.clone();
                ctx.reset();
                ctx.input_ts = Timestamp::unstarted();
            }
            let rc = self.call_kernel(i, KernelPhase::Open);
            if rc != 0 {
                let e = unsafe { node.ctx() }.take_error(rc);
                self.shared.record_error(e.clone());
                return Err(e);
            }
            node.sched.lock().expect("调度锁中毒").opened = true;
        }

        self.set_state(State::Running);
        Ok(())
    }

    fn send(&self, edge: EdgeId, pkt: Packet, blocking: bool) -> Result<()> {
        match self.state() {
            State::Running => {}
            State::Draining | State::Terminated => return Err(Error::Closed),
            s => {
                return Err(Error::State(format!(
                    "send 需要图处于 Running(当前 {s:?});请先调用 start"
                )))
            }
        }
        if self.shared.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(e) = self.shared.first_error() {
            return Err(e);
        }
        if self.edges[edge].is_closed() {
            return Err(Error::Closed);
        }
        // 图输入口上必须有明确时间戳
        if pkt.timestamp() == Timestamp::unset() {
            return Err(Error::InvalidArg(
                "图输入口上的包必须带明确时间戳(UNSET 非法)".into(),
            ));
        }

        // 全局水位:超限时把压力转化成图输入口背压(§7.5)
        while self.shared.over_watermark() {
            if !blocking {
                return Err(Error::WouldBlock);
            }
            if !self.pump_step() {
                // 推不动又降不下来 —— 报错而不是永久阻塞
                return Err(Error::WouldBlock);
            }
        }

        // 时间戳单调性:图输入口强制校验(ADR #23)
        self.check_input_monotonic(edge, &pkt)?;

        // 分发给该边的所有消费者(各自一份引用)与 poller/observer
        self.dispatch(edge, vec![pkt]);
        self.schedule_consumers(edge);
        Ok(())
    }

    /// 图输入口的时间戳必须严格递增(ADR #23)。
    ///
    /// 参照值单独记录在边上,而不是看队列里剩什么 —— 否则队列一排空,
    /// 回退甚至重复的时间戳就能悄悄混进来,下游行为随之变得难以解释。
    fn check_input_monotonic(&self, edge: EdgeId, pkt: &Packet) -> Result<()> {
        let e = &self.edges[edge];
        let mut last = e.last_sent.lock().expect("时间戳锁中毒");
        if *last != Timestamp::unset() && pkt.timestamp() <= *last {
            return Err(Error::InvalidArg(format!(
                "图输入口 `{}` 的时间戳必须严格递增:上一个 {},本次 {}",
                e.name,
                *last,
                pkt.timestamp()
            )));
        }
        *last = pkt.timestamp();
        Ok(())
    }

    /// 把一批包投递到边的消费者与订阅者。
    fn dispatch(&self, edge_id: EdgeId, packets: Vec<Packet>) {
        let edge = &self.edges[edge_id];

        // 订阅者(poller / observer)各自独立一份
        {
            let pollers = edge.pollers.lock().expect("poller 列表锁中毒");
            for p in pollers.iter() {
                for pkt in &packets {
                    p.push(pkt.clone());
                }
            }
        }
        {
            let observers = edge.observers.lock().expect("observer 列表锁中毒");
            for o in observers.iter() {
                for pkt in &packets {
                    let ffi = crate::ffi::borrow_packet(pkt);
                    unsafe { (o.cb)(o.user, ffi) };
                }
            }
        }

        // 内部消费者:每个输入口一份(仅克隆引用计数)
        for &(node, port) in &edge.consumers {
            let mut q = self.nodes[node].input_queues[port]
                .lock()
                .expect("队列锁中毒");
            for pkt in &packets {
                self.shared.on_enqueue(pkt.byte_size());
                q.push_back(pkt.clone());
            }
            let depth = q.len();
            drop(q);
            self.warn_if_over_soft_limit(edge_id, depth);
        }
    }

    /// 内部边只有软水位:超了告警,但**不阻塞生产者**(§7.5)。
    fn warn_if_over_soft_limit(&self, edge_id: EdgeId, depth: usize) {
        let limit = self.shared.config.max_queue_size;
        if limit == 0 || depth <= limit {
            return;
        }
        // 指数退避,避免日志洪水:depth 恰为 limit 的 2^k 倍时才打
        let ratio = depth / limit;
        if ratio.is_power_of_two() {
            runtime::log_warn(&format!(
                "边 `{}` 积压 {} 个包(软水位 {}),消费端可能跟不上",
                self.edges[edge_id].name, depth, limit
            ));
        }
    }

    fn schedule_consumers(&self, edge: EdgeId) {
        let consumers: Vec<NodeId> = self.edges[edge].consumers.iter().map(|&(n, _)| n).collect();
        for n in consumers {
            if self.try_claim(n) {
                self.main_queue.lock().expect("主队列锁中毒").push_back(n);
            }
        }
    }

    /// 尝试取得节点的独占令牌。
    fn try_claim(&self, n: NodeId) -> bool {
        if self.shared.has_error() || self.shared.is_cancelled() {
            return false;
        }
        let node = &self.nodes[n];
        let mut s = node.sched.lock().expect("调度锁中毒");
        if !s.opened || s.closed {
            return false;
        }
        if s.running {
            s.rescan = true; // 合并唤醒:跑完再重扫,否则丢唤醒
            return false;
        }
        drop(s);
        if !node.all_inputs_have_data() {
            return false;
        }
        let mut s = node.sched.lock().expect("调度锁中毒");
        if s.running {
            s.rescan = true;
            return false;
        }
        s.running = true;
        true
    }

    /// 执行一步:跑一个主线程任务,或推进关流。返回是否真的做了事。
    fn pump_step(&self) -> bool {
        // ⚠ 必须先把 pop 的结果落到局部变量:在 edition 2021 里,`if let` 表达式中的
        // 临时值(此处是 MutexGuard)会存活到整个 if-let 块结束 —— 那样 run_node
        // 内部再去 main_queue.lock() 就自锁死。这也是设计文档 R2 锁序规则的实例。
        let next = self.main_queue.lock().expect("主队列锁中毒").pop_front();
        if let Some(n) = next {
            self.run_node(n);
            return true;
        }
        self.try_advance_closing()
    }

    fn run_node(&self, n: NodeId) {
        let node = &self.nodes[n];

        // 取输入:每个口弹一个,取 min 时间戳作为本次的 input_ts
        let mut ts = Timestamp::done();
        {
            let ctx = unsafe { node.ctx() };
            ctx.reset();
            for port in 0..node.input_queues.len() {
                let pkt = node.input_queues[port]
                    .lock()
                    .expect("队列锁中毒")
                    .pop_front();
                if let Some(p) = pkt {
                    self.shared.on_dequeue(p.byte_size());
                    if p.timestamp() < ts {
                        ts = p.timestamp();
                    }
                    ctx.inputs[port] = Some(p);
                }
                ctx.inputs_done[port] =
                    node.input_closed[port].load(Ordering::SeqCst) && node.queue_len(port) == 0;
            }
            ctx.input_ts = ts;
        }

        // 契约校验:算子在 GetContract 里声明的输入类型必须匹配(0 = 接受任意)
        if let Err(e) = self.check_input_types(n) {
            unsafe { node.ctx() }.discard_staging();
            node.stats.lock().expect("统计锁中毒").errors += 1;
            self.shared.record_error(e);
            self.finish(n);
            return;
        }

        let rc = self.call_kernel(n, KernelPhase::Process);

        if rc != 0 {
            let e = {
                let ctx = unsafe { node.ctx() };
                ctx.discard_staging(); // 失败不传播半成品(§7.7)
                ctx.take_error(rc)
            };
            node.stats.lock().expect("统计锁中毒").errors += 1;
            self.shared.record_error(e);
        } else {
            node.stats.lock().expect("统计锁中毒").processed += 1;
            self.flush_staging(n);
        }
        // 立刻释放本次输入的引用 —— 否则上游会一直持着已处理完的包,
        // 使下游的 CoW 永远看到引用数 ≥ 2 而退化成全量拷贝(见 Context::clear_inputs)。
        unsafe { node.ctx() }.clear_inputs();
        self.finish(n);
    }

    /// 契约声明的输入类型校验。类型不符宁可报错,也不让算子按错误类型解读内存。
    fn check_input_types(&self, n: NodeId) -> Result<()> {
        let node = &self.nodes[n];
        let ctx = unsafe { node.ctx() };
        for (port, &want) in node.input_types.iter().enumerate() {
            if want == 0 {
                continue; // 未声明类型 = 接受任意
            }
            let Some(pkt) = ctx.inputs.get(port).and_then(|s| s.as_ref()) else {
                continue;
            };
            if pkt.is_empty() {
                continue; // 空包(时间戳边界)不参与类型校验
            }
            let got = pkt.type_id();
            if got != want {
                return Err(Error::Kernel(format!(
                    "[{}] 输入口 `{}` 类型不符:契约声明 {}, 实际 {}",
                    node.name,
                    node.in_ports.name(port).unwrap_or("?"),
                    crate::packet::type_name(want),
                    crate::packet::type_name(got),
                )));
            }
        }
        Ok(())
    }

    /// 把暂存区的输出分发到下游(此时不持有任何算子回调栈)。
    fn flush_staging(&self, n: NodeId) {
        let node = &self.nodes[n];
        let batches: Vec<(EdgeId, Vec<Packet>)> = {
            let ctx = unsafe { node.ctx() };
            node.outputs
                .iter()
                .enumerate()
                .map(|(i, &e)| (e, std::mem::take(&mut ctx.staging[i])))
                .collect()
        };
        for (edge, packets) in batches {
            if packets.is_empty() {
                continue;
            }
            self.dispatch(edge, packets);
            self.schedule_consumers(edge);
        }
    }

    fn finish(&self, n: NodeId) {
        let node = &self.nodes[n];
        let again = {
            let mut s = node.sched.lock().expect("调度锁中毒");
            s.running = false;
            let a = s.rescan;
            s.rescan = false;
            a
        };
        if (again || node.all_inputs_have_data()) && self.try_claim(n) {
            self.main_queue.lock().expect("主队列锁中毒").push_back(n);
        }
        self.maybe_close(n);
    }

    /// 关流推进:所有输入已关且排空 → close 算子 → 关自己的输出边 → 递归下游。
    fn maybe_close(&self, n: NodeId) -> bool {
        let node = &self.nodes[n];
        {
            let s = node.sched.lock().expect("调度锁中毒");
            if s.closed || s.running || !s.opened {
                return false;
            }
        }
        let force = self.shared.has_error() || self.shared.is_cancelled();
        if !force && !node.all_inputs_closed_and_drained() {
            return false;
        }

        {
            let ctx = unsafe { node.ctx() };
            ctx.reset();
            ctx.close_reason = self.shared.close_reason();
            ctx.input_ts = Timestamp::done();
        }
        let rc = self.call_kernel(n, KernelPhase::Close);
        if rc != 0 {
            let ctx = unsafe { node.ctx() };
            let e = ctx.take_error(rc);
            ctx.discard_staging();
            self.shared.record_error(e);
        } else {
            self.flush_staging(n);
        }
        unsafe { node.ctx() }.clear_inputs();
        node.sched.lock().expect("调度锁中毒").closed = true;

        for &e in &node.outputs {
            self.close_edge(e);
        }
        true
    }

    fn close_edge(&self, edge: EdgeId) {
        let e = &self.edges[edge];
        if e.closed.swap(true, Ordering::SeqCst) {
            return; // 已关
        }
        for &(node, port) in &e.consumers {
            self.nodes[node].input_closed[port].store(true, Ordering::SeqCst);
        }
        // 该边的 poller 在队列排空后即视为结束
        for p in e.pollers.lock().expect("poller 列表锁中毒").iter() {
            p.closed.store(true, Ordering::SeqCst);
        }
    }

    fn set_state_draining_if_all_inputs_closed(&self) {
        let all = self.graph_inputs.iter().all(|&e| self.edges[e].is_closed());
        if all {
            let mut st = self.state.lock().expect("状态锁中毒");
            if *st == State::Running {
                *st = State::Draining;
            }
        }
    }

    /// 尝试推进任一节点的关流;返回是否有进展。
    fn try_advance_closing(&self) -> bool {
        let mut progressed = false;
        for n in 0..self.nodes.len() {
            if self.maybe_close(n) {
                progressed = true;
            }
        }
        if !progressed && self.all_nodes_closed() {
            self.set_state(State::Terminated);
        }
        progressed
    }

    fn all_nodes_closed(&self) -> bool {
        self.nodes
            .iter()
            .all(|n| n.sched.lock().expect("调度锁中毒").closed)
    }

    fn wait_done(&self) -> Result<()> {
        // 主线程执行器:在此借用宿主线程把剩余任务跑完
        loop {
            if self.all_nodes_closed() {
                break;
            }
            if !self.pump_step() {
                break;
            }
        }
        // 排空后再尝试一轮关流
        while self.try_advance_closing() {}
        if self.all_nodes_closed() {
            self.set_state(State::Terminated);
        }
        if self.shared.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match self.shared.first_error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 调用算子回调。**调用期间不持有任何引擎锁**(R1),并记录耗时以便定位卡死。
    fn call_kernel(&self, n: NodeId, phase: KernelPhase) -> i32 {
        let node = &self.nodes[n];
        // 直接交出 UnsafeCell 内部指针:不构造 Rust 引用,故与回调内
        // 从该指针造出的 `&mut Context` 不冲突(独占性由令牌保证)。
        let ctx_ptr = node.ctx.get() as *mut c_void;
        *node.running_since.lock().expect("计时锁中毒") = Some(Instant::now());
        let started = Instant::now();

        // 安全性:ctx_ptr 来自本节点 UnsafeCell,且此刻持有该节点的独占令牌(R3)
        let rc = unsafe {
            match phase {
                KernelPhase::Open => node.kernel.open(ctx_ptr),
                KernelPhase::Process => node.kernel.process(ctx_ptr),
                KernelPhase::Close => node.kernel.close(ctx_ptr),
            }
        };

        let us = started.elapsed().as_micros() as i64;
        *node.running_since.lock().expect("计时锁中毒") = None;
        if matches!(phase, KernelPhase::Process) {
            let mut st = node.stats.lock().expect("统计锁中毒");
            st.total_us += us;
            if us > st.max_us {
                st.max_us = us;
            }
        }
        let wd = self.shared.config.watchdog_ms;
        if wd > 0 && us as u64 > wd * 1000 {
            runtime::log_warn(&format!(
                "节点 `{}` 单次 {:?} 耗时 {} ms,超过 watchdog {} ms",
                node.name,
                phase,
                us / 1000,
                wd
            ));
        }
        rc
    }

    fn queue_depth(&self, edge: EdgeId) -> usize {
        self.edges[edge]
            .consumers
            .iter()
            .map(|&(n, p)| self.nodes[n].queue_len(p))
            .sum()
    }

    fn node_stats(&self, i: usize) -> Option<NodeStatsSnapshot> {
        let node = self.nodes.get(i)?;
        let st = node.stats.lock().expect("统计锁中毒");
        let since = *node.running_since.lock().expect("计时锁中毒");
        Some(NodeStatsSnapshot {
            node_name: node.name.clone(),
            kernel_name: node.kernel_name.clone(),
            running: since.is_some(),
            running_for_us: since.map_or(0, |t| t.elapsed().as_micros() as i64),
            processed: st.processed,
            errors: st.errors,
            total_process_us: st.total_us,
            max_process_us: st.max_us,
            queued: (0..node.input_queues.len())
                .map(|p| node.queue_len(p))
                .sum(),
        })
    }

    fn dump(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "graph state={:?} nodes={} edges={} queued={} ({} bytes)\n",
            self.state(),
            self.nodes.len(),
            self.edges.len(),
            self.shared.total_queued(),
            self.shared.total_queued_bytes()
        ));
        s.push_str("node              state        queued  processed  errors  avg_us  max_us\n");
        for i in 0..self.nodes.len() {
            let st = self.node_stats(i).expect("节点存在");
            let sched = self.nodes[i].sched.lock().expect("调度锁中毒");
            let state = if st.running {
                format!("RUNNING {}ms", st.running_for_us / 1000)
            } else if sched.closed {
                "closed".to_string()
            } else if sched.opened {
                "idle".to_string()
            } else {
                "new".to_string()
            };
            let avg = if st.processed > 0 {
                st.total_process_us / st.processed as i64
            } else {
                0
            };
            s.push_str(&format!(
                "{:<17} {:<12} {:>6}  {:>9}  {:>6}  {:>6}  {:>6}\n",
                st.node_name, state, st.queued, st.processed, st.errors, avg, st.max_process_us
            ));
        }
        for e in &self.edges {
            s.push_str(&format!(
                "edge `{}` closed={} dropped={}\n",
                e.name,
                e.is_closed(),
                e.dropped_count()
            ));
        }
        s
    }

    pub fn add_observer(
        &self,
        port: &str,
        cb: unsafe extern "C" fn(*mut c_void, crate::ffi::FlowPacket),
        user: *mut c_void,
    ) -> Result<()> {
        let edge = *self
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("图输出口 `{port}` 不存在")))?;
        self.edges[edge]
            .observers
            .lock()
            .expect("observer 列表锁中毒")
            .push(Observer { cb, user });
        Ok(())
    }

    // ffi 层需要的只读访问
    pub fn nodes_len(&self) -> usize {
        self.nodes.len()
    }
    pub fn node_name_at(&self, i: usize) -> Option<&str> {
        self.nodes.get(i).map(|n| n.name.as_str())
    }
    pub fn input_port_name_at(&self, i: usize) -> Option<&str> {
        self.graph_inputs
            .get(i)
            .map(|&e| self.edges[e].name.as_str())
    }
    pub fn output_port_name_at(&self, i: usize) -> Option<&str> {
        self.graph_outputs
            .get(i)
            .map(|&e| self.edges[e].name.as_str())
    }
    pub fn num_input_ports(&self) -> usize {
        self.graph_inputs.len()
    }
    pub fn num_output_ports(&self) -> usize {
        self.graph_outputs.len()
    }
    pub fn edge_id_by_name(&self, name: &str) -> Option<EdgeId> {
        self.edge_by_name.get(name).copied()
    }
    pub fn input_edge_by_name(&self, name: &str) -> Option<EdgeId> {
        self.input_by_name.get(name).copied()
    }
    pub fn queue_depth_by_name(&self, name: &str) -> Option<usize> {
        Some(self.queue_depth(self.edge_id_by_name(name)?))
    }
    pub fn dropped_by_name(&self, name: &str) -> Option<u64> {
        Some(self.edges[self.edge_id_by_name(name)?].dropped_count())
    }
    pub fn send_by_edge(&self, edge: EdgeId, pkt: Packet, blocking: bool) -> Result<()> {
        self.send(edge, pkt, blocking)
    }
    pub fn close_edge_pub(&self, edge: EdgeId) {
        self.close_edge(edge);
        self.set_state_draining_if_all_inputs_closed();
    }
    pub fn state_pub(&self) -> State {
        self.state()
    }
    pub fn dump_pub(&self) -> String {
        self.dump()
    }
    pub fn node_stats_pub(&self, i: usize) -> Option<NodeStatsSnapshot> {
        self.node_stats(i)
    }
    pub fn side_packets_mut(&self) -> &Mutex<BTreeMap<String, Packet>> {
        &self.side_packets
    }
    pub fn start_pub(&self) -> Result<()> {
        self.start()
    }
    pub fn wait_done_pub(&self) -> Result<()> {
        self.wait_done()
    }
    pub fn pump_step_pub(&self) -> bool {
        self.pump_step()
    }
    pub fn all_nodes_closed_pub(&self) -> bool {
        self.all_nodes_closed()
    }
    pub fn graph_inputs(&self) -> &[EdgeId] {
        &self.graph_inputs
    }
}

impl Drop for GraphInner {
    /// 兜底关流:图被直接丢弃(没走 wait_done)时,已 open 的算子仍必须收到 Close,
    /// 否则算子里申请的资源(文件、连接、GPU 上下文)不会被释放。
    fn drop(&mut self) {
        for n in 0..self.nodes.len() {
            let need_close = {
                let s = self.nodes[n].sched.lock().expect("调度锁中毒");
                s.opened && !s.closed
            };
            if !need_close {
                continue;
            }
            {
                // 安全性:此刻只有 drop 这一条执行流,独占成立
                let ctx = unsafe { self.nodes[n].ctx() };
                ctx.reset();
                ctx.close_reason = self.shared.close_reason();
                ctx.input_ts = Timestamp::done();
            }
            let rc = self.call_kernel(n, KernelPhase::Close);
            if rc != 0 {
                runtime::log_warn(&format!(
                    "节点 `{}` 在图销毁时的 close 返回 {rc}(已忽略)",
                    self.nodes[n].name
                ));
            }
            unsafe { self.nodes[n].ctx() }.clear_inputs();
            self.nodes[n].sched.lock().expect("调度锁中毒").closed = true;
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum KernelPhase {
    Open,
    Process,
    Close,
}
