//! The graph: topology, validation, execution and stream shutdown.
//!
//! Structurally this is an **index arena**: entities live in `Vec`s and refer to each other by
//! `usize` id (see `docs/design.md` §6.1), which avoids a graph of self-referential pointers.
//!
//! Queues belong to a **consumer's input port**, not to an edge: one edge may feed several
//! consumers and each of them must receive every packet, so a shared queue would make them steal
//! packets from one another. An edge therefore only carries topology and closed-state, and packets
//! are dispatched per consumer — cloning a reference, never the payload.

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::config::{GraphConfig, StatsLevel};
use crate::context::Context;
use crate::executor::Executor;
use crate::kernel::{KernelInstance, PortTable};
use crate::packet::Packet;
use crate::runtime::{self, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

mod api;
mod backpressure;
mod build;
mod dot;
mod introspect;
mod lifecycle;
mod node;
mod poller;
mod scheduler;

use backpressure::*;
use node::*;
pub use node::{DotView, State};
use poller::PollerInner;
pub use poller::{
    OutputEvent, Poller, PollerBackpressureStatsSnapshot, PollerOptions, PollerOverflow,
};

pub type NodeId = usize;
pub type EdgeId = usize;

// ---------------------------------------------------------------- 图

/// 「有进展」的通知状态:活动代数 + **当前阻塞在 condvar 上的宿主线程数**。
///
/// `waiters` 与 `gen` 同锁保护,这是能安全跳过 `notify_all` 的依据:notifier 在持锁时
/// 读到 `waiters == 0`,则任何「正要等待」的线程此刻都还没拿到这把锁 —— 它随后会看到
/// 递增后的 `gen`(≠ 它捕获的 `before`)从而根本不进入等待。故**不会丢唤醒**。
///
/// 为什么值得为此加一个计数:`notify_activity` 在 `dispatch` 里是**每包每条边**调一次,
/// 而 `Condvar::notify_all` 即使没有任何等待者也会走一次 futex 系统调用
/// (本机实测约 372 ns,与 `getpid()` 的 329 ns 同量级 —— 该机器系统调用被放大约 5 倍,
/// 裸机约 60~80 ns)。没人在等的时候这纯属白付。
#[derive(Debug, Default)]
struct Activity {
    gen: u64,
    waiters: usize,
}

type WakeupCallback = Arc<dyn Fn() + Send + Sync>;

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
    /// 所有执行器；委托执行器自持交还宿主线程执行的任务队列。
    executors: Vec<Executor>,
    /// 已投递到执行器、尚未跑完的任务数。用于「是否空闲」与终止判定。
    in_flight: AtomicUsize,
    /// 委托任务抽取游标。多个 DelegatingExecutor 之间轮询，避免前面的队列长期饿死后面的。
    delegated_cursor: AtomicUsize,
    /// 同一时刻只允许一个宿主线程执行委托任务，兑现 DelegatingExecutor 的零并发语义。
    delegated_running: AtomicBool,
    /// 有任何进展时唤醒阻塞中的宿主线程(取到输出、节点关闭、出错、任务入队)
    activity: (Mutex<Activity>, Condvar),
    /// 事件循环唤醒回调。只负责通知宿主“该进引擎推进了”，不得承载实际工作。
    wakeup_callback: Mutex<Option<WakeupCallback>>,
    /// 合并重复通知：宿主调用 `pump_step` 确认一次唤醒后重新武装。
    wakeup_pending: AtomicBool,
    /// 每次唤醒请求递增，避免活动恰好发生在 `pump_step(false)` 重新武装窗口时丢通知。
    wakeup_generation: AtomicU64,
    /// 暂停调度(调试/限速)。已在执行的算子不受影响。
    paused: AtomicBool,
    /// 因下游无损输入队列已满而保留 staging 的节点。下游每次出队后重试这些刷新。
    blocked_flush_nodes: Mutex<BTreeSet<NodeId>>,
    side_packets: Mutex<BTreeMap<String, Packet>>,
    /// 各算子在 GetContract 里声明的必需 side packet:(名字, 声明它的节点)
    required_side_packets: Vec<(String, String)>,
    /// 计时基准。`Instant` 无法放进原子,故节点统计里存「相对本基准的微秒」。
    epoch: Instant,
    /// 本轮 `start` 的时刻(相对 `epoch` 的微秒 + 1);0 表示尚未开始。
    run_started_us: AtomicI64,
    /// Compact / Diagnostics 各自相邻两次导出之间的私有基线；不作为宿主查询 API 暴露。
    dot_intervals: Mutex<dot::DotIntervalBaselines>,
    /// 建图时确定，运行期间不变，故无需原子。
    stats_level: StatsLevel,
}

impl GraphInner {
    #[inline]
    fn basic_stats(&self) -> bool {
        self.stats_level != StatsLevel::Off
    }

    #[inline]
    fn full_stats(&self) -> bool {
        self.stats_level == StatsLevel::Full
    }
}

/// A handle to a computation graph.
///
/// Build one with [`from_yaml`](Graph::from_yaml), attach output sinks with
/// [`add_poller`](Graph::add_poller) (pull) or [`observe`](Graph::observe) (push), then
/// [`start`](Graph::start). Feed packets in through [`input`](Graph::input); terminate by closing
/// the inputs ([`close_all_inputs`](Graph::close_all_inputs)) and waiting for the pipeline to
/// drain ([`wait_done`](Graph::wait_done)).
///
/// Every method takes `&self` — the handle is shared, not cloned. Dropping it shuts the graph down
/// and joins the worker threads. After termination, [`reset`](Graph::reset) puts the graph back to
/// a startable state while keeping already-opened kernel instances alive, so an expensive one-off
/// such as loading a model is not repeated.
pub struct Graph {
    inner: Arc<GraphInner>,
}

impl Graph {
    pub fn from_yaml(text: &str) -> Result<Self> {
        let cfg = GraphConfig::from_yaml(text)?;
        Self::from_config(cfg)
    }

    pub fn from_yaml_file(path: &str) -> Result<Self> {
        let cfg = GraphConfig::from_yaml_file(path)?;
        Self::from_config(cfg)
    }

    pub fn from_config(cfg: GraphConfig) -> Result<Self> {
        let inner = GraphInner::build(cfg)?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn state(&self) -> State {
        *self.inner.state.lock().expect("state lock poisoned")
    }

    pub fn inner(&self) -> &Arc<GraphInner> {
        &self.inner
    }

    /// 注入常量输入。必须在 `start` 之前。
    pub fn set_side_packet(&self, name: &str, pkt: Packet) -> Result<()> {
        if self.state() != State::Initialized {
            return Err(Error::State(
                "side packets must be injected before start".into(),
            ));
        }
        if pkt.type_id() == crate::packet::type_id::HOST_OBJECT {
            return Err(Error::InvalidArg(format!(
                "side packet `{name}` carries LMFLOW_TYPE_HOST_OBJECT, which is reserved and \
                 not enabled (see ADR #26); use LMFLOW_TYPE_BUFFER for numeric collections, \
                 or LMFLOW_TYPE_STR carrying JSON for arbitrary metadata"
            )));
        }
        self.inner
            .side_packets
            .lock()
            .expect("side packet lock poisoned")
            .insert(name.to_string(), pkt);
        Ok(())
    }

    pub fn input(&self, port: &str) -> Result<Input> {
        let edge =
            *self.inner.input_by_name.get(port).ok_or_else(|| {
                Error::NotFound(format!("graph input port `{port}` does not exist"))
            })?;
        Ok(Input {
            graph: self.inner.clone(),
            edge,
        })
    }

    pub fn close_input(&self, port: &str) -> Result<()> {
        let edge =
            *self.inner.input_by_name.get(port).ok_or_else(|| {
                Error::NotFound(format!("graph input port `{port}` does not exist"))
            })?;
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
        for node in &self.inner.nodes {
            node.source_waiting.store(false, Ordering::SeqCst);
            node.source_wait_reason.store(0, Ordering::Relaxed);
            node.source_wake_deadline_us.store(0, Ordering::Relaxed);
            node.source_wake_generation.fetch_add(1, Ordering::SeqCst);
        }
        for executor in &self.inner.executors {
            executor.clear_delayed();
        }
        self.inner.resume_blocked_flushes();
        self.inner.finish_all_backpressure_blocks();
        self.inner.notify_activity();
        self.inner.request_wakeup();
    }

    pub fn wait_done_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.inner
            .wait_done(Some(std::time::Instant::now() + timeout))
    }

    pub fn wait_until_idle_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.inner
            .wait_until_idle(Some(std::time::Instant::now() + timeout))
    }

    pub fn pause(&self) {
        self.inner.pause();
    }

    pub fn resume(&self) {
        self.inner.resume();
    }

    pub fn is_paused(&self) -> bool {
        self.inner.is_paused()
    }

    /// 已定义的线程池名字(按 YAML 顺序)。
    pub fn executor_names(&self) -> Vec<&str> {
        self.inner.executor_names()
    }

    /// 是否空闲:委托队列为空且所有执行器无在飞任务。
    pub fn is_idle(&self) -> bool {
        self.inner.is_idle_pub()
    }

    /// 在当前宿主线程上执行至多一个委托任务，或推进一次关流。
    ///
    /// 事件循环型宿主可反复调用它主动推进 `DelegatingExecutor`，而不必进入阻塞接口。
    pub fn pump_step(&self) -> bool {
        self.inner.pump_step_pub()
    }

    /// Install an edge-triggered event-loop wakeup callback.
    ///
    /// The callback may run on any engine thread and must return quickly. It should only schedule
    /// the host loop to call [`pump_step`](Self::pump_step) until it returns `false`.
    ///
    /// # There is only one slot
    ///
    /// A graph holds a *single* callback: installing again **replaces** the previous one rather
    /// than adding to it, and the notification is graph-global — it says "something progressed",
    /// not which port produced output or which queue freed a slot.
    ///
    /// So a host that wants several independent wakeup consumers (waiting for the run to finish,
    /// for a port to emit, for an input queue to drain) must fan out itself: one owner installs
    /// this callback and broadcasts to the waiters, each re-checking its own condition. Calling
    /// this a second time from elsewhere silently displaces the first callback, and the symptom
    /// is a graph that stops advancing — no error, no log.
    pub fn set_wakeup_callback<F>(&self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.inner.set_wakeup_callback(Some(Arc::new(callback)));
    }

    /// Remove the event-loop wakeup callback.
    pub fn clear_wakeup_callback(&self) {
        self.inner.set_wakeup_callback(None);
    }

    /// 推模式订阅输出口。回调在派发该包的线程上执行(可能是线程池线程)。
    ///
    /// 必须在 `start` 之前注册,否则会漏掉已产出的包。
    pub fn observe<F>(&self, port: &str, f: F) -> Result<()>
    where
        F: Fn(&Packet) + Send + Sync + 'static,
    {
        self.observe_inner(port, f, false)
    }

    /// 推模式订阅输出口，同时接收以空包编码的时间戳边界事件。
    pub fn observe_with_timestamp_bounds<F>(&self, port: &str, f: F) -> Result<()>
    where
        F: Fn(&Packet) + Send + Sync + 'static,
    {
        self.observe_inner(port, f, true)
    }

    fn observe_inner<F>(&self, port: &str, f: F, observe_timestamp_bounds: bool) -> Result<()>
    where
        F: Fn(&Packet) + Send + Sync + 'static,
    {
        if self.state() != State::Initialized {
            return Err(Error::State(
                "observe must be called before start, otherwise already-produced packets are missed".into(),
            ));
        }
        self.inner
            .add_observer_fn(port, Arc::new(f), observe_timestamp_bounds)
    }

    /// 算子自报计数器的当前值(按图隔离)。
    pub fn counter_value(&self, name: &str) -> i64 {
        self.inner.shared.counter_value(name)
    }

    /// 已登记的计数器名。
    pub fn counter_names(&self) -> Vec<String> {
        self.inner.shared.counter_names()
    }

    /// 取出图级共享状态的句柄 —— 它比 `Graph` 句柄活得久,
    /// 因此可以在图销毁**之后**仍然读取错误与计数器(测试与排障用)。
    pub fn shared_for_inspection(&self) -> Arc<crate::runtime::GraphShared> {
        self.inner.shared.clone()
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

    /// 导出 Graphviz DOT(拓扑 + 子图命名空间 cluster + 执行器/绑核图例)。见 `GraphInner::to_dot`。
    pub fn to_dot(&self) -> String {
        self.to_dot_with_view(DotView::Topology)
    }

    /// 导出指定详细程度的 Graphviz DOT。
    pub fn to_dot_with_view(&self, view: DotView) -> String {
        self.inner.to_dot(view)
    }

    /// 带节点状态与核心统计的紧凑视图,适合大型图持续刷新。
    pub fn to_dot_compact(&self) -> String {
        self.to_dot_with_view(DotView::Compact)
    }

    /// 同 [`to_dot`](Self::to_dot),但在每个节点标签上标出运行统计
    /// (处理数 · 平均延迟 · 收/发包数 · 队列峰值 · 错误数),并把填充色换成
    /// **按平均延迟的热力图**(绿=快 → 红=慢)—— 一眼看出瓶颈在哪个节点。
    ///
    /// 可在图运行期间随时调用(统计是原子读的快照),不必等跑完。
    /// 注意:热力图占用了「按执行器上色」那一维,执行器仍以标签里的 `@name` 标出。
    pub fn to_dot_with_stats(&self) -> String {
        self.to_dot_with_view(DotView::Diagnostics)
    }

    pub fn node_stats(&self, i: usize) -> Option<NodeStatsSnapshot> {
        self.inner.node_stats(i)
    }

    pub fn input_queue_stats(&self, node: usize, port: usize) -> Option<InputQueueStatsSnapshot> {
        self.inner.input_queue_stats(node, port)
    }
}

impl Drop for Graph {
    /// 在宿主放开句柄时**就地关停线程池并 join**。
    ///
    /// 必须在这里做,不能只依赖 `GraphInner::drop`:工作线程为了执行节点会
    /// `Weak::upgrade` 出一个临时强引用,若宿主的 Arc 已先释放,最后一个引用就落在
    /// 工作线程手上 —— 于是 `GraphInner::drop` 在工作线程上运行,`shutdown` 变成
    /// **join 自己**,得到 `EDEADLK`。先在宿主线程 join 完,worker 就不存在了。
    fn drop(&mut self) {
        self.inner.shutdown_executors_pub();
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
    pub fn backpressure_stats(&self) -> WatermarkBackpressureStatsSnapshot {
        self.graph.watermark_backpressure_stats(self.edge)
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

/// `LMFlowNodeStats` 的 Rust 侧快照。
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
    /// 从输入口取走的包数
    pub packets_in: u64,
    /// 产出并派发下游的包数
    pub packets_out: u64,
    /// 下游入队时观察到的队列深度峰值(高水位)
    pub peak_queue_depth: usize,
    pub queued: usize,
}

#[derive(Debug, Clone)]
pub(super) struct ExecutorStatsSnapshot {
    pub queued: usize,
    pub running: usize,
    pub peak_queued: usize,
    pub completed: u64,
    pub total_wait_us: u64,
    pub total_execution_us: u64,
    pub queued_for_us: u64,
    pub saturated: bool,
    pub queued_nodes: Vec<String>,
}

/// 节点单个输入口的队列与背压统计快照。
#[derive(Debug, Clone)]
pub struct InputQueueStatsSnapshot {
    pub node_name: String,
    pub port_name: String,
    pub producer_name: Option<String>,
    pub packet_capacity: Option<usize>,
    pub queued_packets: usize,
    pub queued_bytes: u64,
    pub reserved_packets: usize,
    pub peak_queued_packets: usize,
    pub peak_queued_bytes: u64,
    pub blocked: bool,
    pub blocked_for_us: u64,
    pub block_events: u64,
    pub total_blocked_us: u64,
}
#[derive(Debug, Clone)]
pub struct WatermarkBackpressureStatsSnapshot {
    pub port_name: String,
    pub packet_limit: usize,
    pub total_queued_packets: usize,
    pub blocked: bool,
    pub active_waiters: usize,
    pub blocked_for_us: u64,
    pub block_events: u64,
    pub total_blocked_us: u64,
}
