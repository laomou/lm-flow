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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::config::GraphConfig;
use crate::context::Context;
use crate::executor::ThreadPool;
use crate::kernel::{KernelInstance, PortTable};
use crate::packet::Packet;
use crate::runtime::{self, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

mod build;
mod dot;
mod introspect;

pub type NodeId = usize;
pub type EdgeId = usize;

/// 输入策略(节点级可插拔,见 docs/design.md §7.10)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPolicy {
    /// 所有输入口齐备才触发。默认。
    Sync,
    /// 任一输入口有数据即触发,不等其它口。适合无需对齐的旁路处理。
    Immediate,
    /// 队列有界:**满则丢弃最旧的包**。实时场景必备 ——
    /// 摄像头 30fps 而算子只跑 10fps 时,无界队列会让内存无限增长,丢旧帧才是正确取舍。
    /// 就绪条件同 `Sync`。
    FixedSize { capacity: usize },
    /// **部分对齐**:把输入口划成若干组,每组内各自按时间戳对齐;任一组就绪即触发,
    /// 且本次只带上该组的口(其余口留空)。分组是输入口的一个**完整划分**(每口恰属一组)。
    /// 用于「A、B 该配对,C 独立」这类图。存的是输入口序号(建图时由端口名解析)。
    SyncSet { sets: Vec<Vec<usize>> },
    /// **批处理**:攒够 `size` 个**对齐元组**一次交给算子(`process()` 按批读
    /// `input_count`/`input_at`);关流时不足一批也刷出。多输入口时按时间戳对齐
    /// (与 `Sync` 同源),各口取数可以不同。用于批推理 / 窗口聚合。见 docs/design.md §7.10。
    Batch { size: usize },
}

impl InputPolicy {
    /// 从配置构造。`ins` 用于把 `sync_set` 里的端口**名字**解析成序号并校验。
    fn from_config(
        c: &crate::config::InputPolicyConfig,
        ins: &crate::kernel::PortTable,
    ) -> Result<Self> {
        Ok(match c.r#type.as_str() {
            "immediate" => InputPolicy::Immediate,
            "fixed_size" => InputPolicy::FixedSize {
                capacity: c.capacity.max(1),
            },
            "batch" => InputPolicy::Batch {
                size: c.capacity.max(1),
            },
            "sync_set" => {
                if c.sets.is_empty() {
                    return Err(Error::InvalidArg(
                        "sync_set policy must provide sets (input port groups)".into(),
                    ));
                }
                let mut resolved: Vec<Vec<usize>> = Vec::new();
                let mut seen = vec![false; ins.len()];
                for set in &c.sets {
                    if set.is_empty() {
                        return Err(Error::InvalidArg("sync_set group must not be empty".into()));
                    }
                    let mut group = Vec::with_capacity(set.len());
                    for name in set {
                        let idx = ins.index_by_name(name).ok_or_else(|| {
                            Error::InvalidArg(format!(
                                "sync_set references nonexistent input port `{name}`"
                            ))
                        })?;
                        if seen[idx] {
                            return Err(Error::InvalidArg(format!(
                                "sync_set input port `{name}` appears in multiple groups (groups must be disjoint)"
                            )));
                        }
                        seen[idx] = true;
                        group.push(idx);
                    }
                    resolved.push(group);
                }
                if let Some(miss) = (0..ins.len()).find(|&i| !seen[i]) {
                    return Err(Error::InvalidArg(format!(
                        "sync_set must cover all input ports; at least `{}` is missing (put it in its own group to keep it independent)",
                        ins.name(miss).unwrap_or("?")
                    )));
                }
                InputPolicy::SyncSet { sets: resolved }
            }
            _ => InputPolicy::Sync,
        })
    }
}

/// 一次触发的计划:处理时间戳 + 参与本次的输入口。
/// `ports = None` 表示「全部口」(Sync / Immediate / FixedSize 的现状,不分配);
/// `Some(set)` 表示「只这些口」(SyncSet 的就绪组)—— 认领时只对这些口弹包、推进 bound。
struct Ready {
    ts: Timestamp,
    ports: Option<Vec<usize>>,
    /// 仅 `batch` 策略:就绪判定时已算好的取包计划。放在这里而不是认领时重算,
    /// 是为了保住「每口只拿一次队列锁」(ADR #36)—— 判定期已把各口时间戳前缀
    /// 快照过一次,认领期照计划批量弹出即可,不必再逐轮加锁。
    batch: Option<BatchPlan>,
}

/// `batch` 策略的认领计划:每个**正向口**本次取多少个包,以及本批末尾的对齐时间戳。
///
/// 各口取数**可以不同** —— 某口在某个对齐时间戳上没有包,该轮就不取它。这与 `sync`
/// 单包时的语义一致(`Context::input_count` 本就是按口计数的),而不是「各口各自数够
/// `size` 个」:后者会把 0 号口的第 k 个与 1 号口的第 k 个配成一对,而它们未必是同一帧,
/// 属于**静默的错误配对**。
struct BatchPlan {
    /// (端口号, 取包数)
    take: Vec<(usize, usize)>,
    /// 本批最后一轮对齐到的时间戳:用作 `input_ts`,并据此推进各口 bound。
    last_ts: Timestamp,
}

// ---------------------------------------------------------------- 状态机

/// 与 `LMFlowGraphState` 一致。
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
    /// 纯诊断计数器(经 `lmflow_graph_dropped_count` 读回),不承载任何 happens-before
    /// → `Relaxed`。注意上面的 `closed` 是**真同步**(参与终止判定),必须留 SeqCst。
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// 推模式订阅者。C 宿主给函数指针,Rust 宿主给闭包 —— 后者才是 Rust 侧的自然写法。
#[derive(Clone)]
enum Observer {
    C {
        cb: unsafe extern "C" fn(*mut c_void, crate::ffi::LMFlowPacket),
        user: *mut c_void,
    },
    Rust(Arc<dyn Fn(&Packet) + Send + Sync>),
}
// 安全性:user 是宿主的不透明指针,引擎只原样回传。
unsafe impl Send for Observer {}

// ---------------------------------------------------------------- poller

pub struct PollerInner {
    edge: EdgeId,
    edge_name: String,
    queue: Mutex<VecDeque<Packet>>,
    closed: AtomicBool,
    capacity: Option<usize>,
    overflow: PollerOverflow,
    dropped: AtomicU64,
    active: AtomicBool,
}

impl PollerInner {
    fn push(&self, graph: &GraphInner, p: Packet) -> bool {
        loop {
            if !self.active.load(Ordering::SeqCst) {
                return false;
            }
            let before = graph.activity_gen();
            let mut queue = self.queue.lock().expect("poller lock poisoned");
            if !self.active.load(Ordering::SeqCst) {
                return false;
            }
            let full = self
                .capacity
                .is_some_and(|capacity| queue.len() >= capacity);
            if !full {
                graph.shared.on_enqueue(p.byte_size());
                queue.push_back(p);
                return true;
            }
            match self.overflow {
                PollerOverflow::Block => {
                    drop(queue);
                    if !self.active.load(Ordering::SeqCst)
                        || graph.shared.is_cancelled()
                        || graph.shared.has_error()
                    {
                        return false;
                    }
                    graph.wait_activity_since(before, std::time::Duration::from_millis(100));
                }
                PollerOverflow::DropOldest => {
                    if let Some(old) = queue.pop_front() {
                        graph.shared.on_dequeue(old.byte_size());
                        self.note_dropped(1);
                    }
                    graph.shared.on_enqueue(p.byte_size());
                    queue.push_back(p);
                    return true;
                }
                PollerOverflow::DropNewest => {
                    drop(queue);
                    self.note_dropped(1);
                    return false;
                }
                PollerOverflow::Latest => {
                    let dropped = queue.len() as u64;
                    while let Some(old) = queue.pop_front() {
                        graph.shared.on_dequeue(old.byte_size());
                    }
                    if dropped != 0 {
                        self.note_dropped(dropped);
                    }
                    graph.shared.on_enqueue(p.byte_size());
                    queue.push_back(p);
                    return true;
                }
            }
        }
    }

    fn pop(&self, graph: &GraphInner) -> Option<Packet> {
        let packet = self.queue.lock().expect("poller lock poisoned").pop_front();
        if let Some(packet) = &packet {
            graph.shared.on_dequeue(packet.byte_size());
            graph.notify_activity();
        }
        packet
    }

    fn is_empty(&self) -> bool {
        self.queue.lock().expect("poller lock poisoned").is_empty()
    }

    fn clear(&self, graph: &GraphInner) {
        let mut queue = self.queue.lock().expect("poller lock poisoned");
        while let Some(packet) = queue.pop_front() {
            graph.shared.on_dequeue(packet.byte_size());
        }
        drop(queue);
        graph.notify_activity();
    }

    fn note_dropped(&self, count: u64) {
        let before = self.dropped.fetch_add(count, Ordering::Relaxed);
        let after = before + count;
        if before == 0 || after.is_power_of_two() {
            runtime::log_warn(&format!(
                "poller on edge `{}` has dropped {} packets total due to overflow policy {:?}",
                self.edge_name, after, self.overflow
            ));
        }
    }
}

/// A bounded poller's behavior when its queue reaches capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollerOverflow {
    Block,
    DropOldest,
    DropNewest,
    Latest,
}

/// Options for [`Graph::add_poller_with_options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollerOptions {
    pub capacity: usize,
    pub overflow: PollerOverflow,
}

impl PollerOptions {
    pub fn new(capacity: usize, overflow: PollerOverflow) -> Self {
        Self { capacity, overflow }
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
        self.next_deadline(None).ok().flatten()
    }

    /// 带超时:`Ok(Some)` 取到,`Ok(None)` 图已结束,`Err(Timeout)` 超时。
    pub fn next_timeout(&self, timeout: std::time::Duration) -> Result<Option<Packet>> {
        self.next_deadline(Some(std::time::Instant::now() + timeout))
    }

    fn next_deadline(&self, deadline: Option<std::time::Instant>) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.inner.pop(&self.graph) {
                return Ok(Some(p));
            }
            if self.inner.closed.load(Ordering::SeqCst) || self.graph.shared.has_error() {
                // 先把队列排干再宣告结束
                return Ok(self.inner.pop(&self.graph));
            }
            // 有主线程任务就顺手跑掉(默认执行器就是宿主线程)
            if self.graph.pump_step() {
                continue;
            }
            // 在判断空闲**之前**捕获活动代数(防丢唤醒,见 GraphInner::activity_gen)。
            let before = self.graph.activity_gen_pub();
            if self.graph.is_idle_pub() {
                // 主线程与线程池都空了 —— 不会再有新输出
                return Ok(self.inner.pop(&self.graph));
            }
            // 线程池还在跑,等它有进展
            match self.graph.remaining_for_poller(deadline) {
                Some(d) => {
                    self.graph.wait_activity_since_pub(before, d);
                }
                None => return Err(Error::Timeout),
            }
        }
    }

    /// 非阻塞:仅看现有队列。
    pub fn try_next(&self) -> Option<Packet> {
        self.inner.pop(&self.graph)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------- 节点

/// 节点调度状态。支持 `max_in_flight` 个并行 in-flight 调用。
///
/// 并行模型(docs/design.md §7.11):
///  * 认领(try_claim)在锁下**原子地**取一个 context 槽、按对齐时间戳弹入本次输入、
///    分配一个递增序号 `seq`。序号即消费时间戳的顺序(因为总取最小就绪时间戳)。
///  * 调用完成后按 `seq` **重排刷新**:只有轮到 `next_flush_seq` 时才把输出投递下游,
///    从而即使后面的时间戳先算完,下游看到的时间戳依然单调。
///  * `in_flight` = 已认领但尚未刷新的数量 = 占用中的槽数;它归零节点才可关闭。
///
/// `max_in_flight == 1` 是自然特例:只有一个槽,序号恒连续,重排是恒等操作 ——
/// 行为与串行路径一致。
#[derive(Debug)]
struct NodeSched {
    opened: bool,
    /// 已「认领关流」—— 在锁下置位,保证并发时只有一个线程会调算子的 Close。
    /// 必须与 `closed` 分开:`closed` 表示 Close 已跑完,终止判定看它。
    close_started: bool,
    closed: bool,
    /// 已认领但尚未刷新的调用数(= 占用中的槽数)。归零方可关闭。
    in_flight: usize,
    /// 可用的 context 槽序号(初始 0..max_in_flight)。
    free_slots: Vec<usize>,
    /// 已认领、等待某个 worker 来执行的调用:(slot, seq)。
    ready: VecDeque<(usize, u64)>,
    /// 取时间戳时分配的下一个序号。
    next_seq: u64,
    /// 下一个可刷新的序号(保证下游时间戳单调)。
    next_flush_seq: u64,
    /// 完成但等待按序刷新的调用:seq -> (slot, 是否成功)。
    pending_flush: BTreeMap<u64, (usize, bool)>,
    /// 当前按序轮到、但因下游内部输入队列已满而暂缓刷新的槽。
    blocked_flush: Option<BlockedFlush>,
    /// 是否已有线程在做刷新 —— 保证刷新按序、串行(否则并发刷新会打乱下游顺序)。
    flushing: bool,
}

#[derive(Debug, Clone, Copy)]
enum BlockedFlush {
    Invocation { slot: usize, ok: bool },
    Close,
}

#[derive(Debug, Clone, Copy)]
struct InputQueueReservation {
    node: NodeId,
    port: usize,
    packets: usize,
    bytes: u64,
}

#[derive(Debug, Default)]
struct InputQueueStats {
    peak_packets: AtomicUsize,
    peak_bytes: AtomicU64,
    block_events: AtomicU64,
    blocked_total_us: AtomicU64,
    /// 0 = 当前未阻塞；否则为相对 graph epoch 的微秒数 + 1。
    blocked_since_us: AtomicI64,
}

impl InputQueueStats {
    fn reset(&self) {
        self.peak_packets.store(0, Ordering::Relaxed);
        self.peak_bytes.store(0, Ordering::Relaxed);
        self.block_events.store(0, Ordering::Relaxed);
        self.blocked_total_us.store(0, Ordering::Relaxed);
        self.blocked_since_us.store(0, Ordering::Relaxed);
    }
}

impl NodeSched {
    fn new(max_in_flight: usize) -> Self {
        Self {
            opened: false,
            close_started: false,
            closed: false,
            in_flight: 0,
            free_slots: (0..max_in_flight).collect(),
            ready: VecDeque::new(),
            next_seq: 0,
            next_flush_seq: 0,
            pending_flush: BTreeMap::new(),
            blocked_flush: None,
            flushing: false,
        }
    }
}

/// 节点级运行统计。**全原子、无锁** —— 每包每节点都要更新,放 `Mutex` 里就是在热路径上
/// 加锁(改造前每包要拿 4 次锁:计时进/出 + 耗时 + processed)。
///
/// 计数器用 `Relaxed`:它们不参与任何 happens-before 推理,只被读侧当快照看。
/// `max_in_flight > 1` 时同一节点会被多个工作线程并发更新,故必须是多写者安全的。
#[derive(Debug, Default)]
struct NodeStats {
    processed: AtomicU64,
    errors: AtomicU64,
    total_us: AtomicI64,
    max_us: AtomicI64,
    /// 本节点从输入口取走的包数(在 `try_claim` 弹包处累加)
    packets_in: AtomicU64,
    /// 本节点产出并派发下游的包数(在 `flush_staging` 派发处累加)
    packets_out: AtomicU64,
    /// 下游入队时观察到的**队列深度峰值**(高水位)—— 定位积压点
    peak_queue_depth: AtomicUsize,
    /// 正在执行算子回调的并发数(> 0 即「在跑」)
    in_flight: AtomicUsize,
    /// 最近一次 `in_flight` 0→1 跃变的时刻(相对 [`GraphInner::epoch`] 的微秒)。
    /// **归零时不清零** —— 读侧一律先看 `in_flight > 0` 再用它,从而避开
    /// 「清零」与「新一次开始」互相覆盖的竞争(那会让诊断值瞬时错乱)。
    started_us: AtomicI64,
}

impl NodeStats {
    /// 全字段清零,供图 reset 重跑用。仅在图静止时调用(无并发),但字段是内嵌原子,
    /// 故用 `&self` 逐个 store 即可(不需要 `&mut`)。
    fn reset(&self) {
        self.processed.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.total_us.store(0, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
        self.packets_in.store(0, Ordering::Relaxed);
        self.packets_out.store(0, Ordering::Relaxed);
        self.peak_queue_depth.store(0, Ordering::Relaxed);
        self.in_flight.store(0, Ordering::Relaxed);
        self.started_us.store(0, Ordering::Relaxed);
    }
}

/// 算子失败时本节点怎么办(见 `NodeConfig::on_error`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// 记录首个错误、终止全图(默认,历史行为)。
    Abort,
    /// 丢掉出错的那一个包、**推进下游时间戳边界**、计数并打 WARN,然后继续跑。
    ///
    /// 为什么必须推进边界:不推进的话下游会永远等这一刻的数据 ——
    /// 那等于把「一帧出错」升级成「整图卡死」,比 abort 还糟。
    Skip,
}

impl OnError {
    fn from_config(s: &str) -> Self {
        // 未知值已在 config 校验期拒掉,这里只认已知的两个。
        if s == "skip" {
            OnError::Skip
        } else {
            OnError::Abort
        }
    }
}

pub struct Node {
    pub name: String,
    pub kernel_name: String,
    pub inputs: Vec<EdgeId>,
    pub outputs: Vec<EdgeId>,
    pub in_ports: Arc<PortTable>,
    pub out_ports: Arc<PortTable>,
    /// `None` = 宿主主线程(默认执行器,ADR #16);`Some(i)` = executors[i] 线程池
    executor: Option<usize>,
    policy: InputPolicy,
    input_types: Vec<u64>,
    output_types: Vec<u64>,
    kernel: KernelInstance,
    /// 每次并行 in-flight 调用一个 context 槽(池大小 = max_in_flight)。
    /// 用 UnsafeCell 而非 Mutex:算子回调期间引擎必须交出一个 `*mut Context` 给 C 侧,
    /// 若同时持有 Mutex guard 的 `&mut`,回调里从裸指针再造 `&mut` 就构成别名 UB。
    /// 独占性由「一个槽同一时刻只被一个调用持有」保证(槽在锁下认领/归还)。
    /// 池在 build 后不再增长,故元素地址稳定 —— 交给 C 侧的 `*mut Context` 在
    /// 调用期间始终有效。
    ctxs: Vec<UnsafeCell<Context>>,
    max_in_flight: usize,
    sched: Mutex<NodeSched>,
    /// 全原子,无锁 —— 见 [`NodeStats`]
    stats: NodeStats,
    /// 每个输入口一条独立队列(见模块头注释)
    input_queues: Vec<Mutex<VecDeque<Packet>>>,
    /// 每个正向输入口的无损包数容量。`None` = 不限。
    input_queue_capacity: Vec<Option<usize>>,
    /// 每个正向输入口的无损 payload 浅字节容量。`None` = 不限。
    input_queue_byte_capacity: Vec<Option<u64>>,
    /// 已由上游刷新预留、尚未真正入队的槽数。与 queue len 合计做并发容量判定。
    input_queue_reserved: Vec<AtomicUsize>,
    /// 已由上游刷新预留、尚未真正入队的字节数。
    input_queue_reserved_bytes: Vec<AtomicU64>,
    /// 当前各输入队列内 payload 的浅字节数。
    input_queue_bytes: Vec<AtomicU64>,
    /// 每个输入口的背压与高水位统计。
    input_queue_stats: Vec<InputQueueStats>,
    input_closed: Vec<AtomicBool>,
    /// 算子失败时的处理策略(见 [`OnError`])。建图期定下,之后不变。
    on_error: OnError,
    /// 源节点定速:相邻两次 `process` 的最小间隔。`None` = 不限速(见 `NodeConfig::rate`)。
    min_period: Option<std::time::Duration>,
    /// 上次 `process` 的开始时刻,配合 `min_period` 节流。仅源节点用到 ——
    /// 源本就串行自续产(一个包跑完才排下一个),故一把 Mutex 足够、无竞争压力。
    last_fire: Mutex<Option<Instant>>,
    /// 每个输入口是否为 back-edge(反馈寄存器):true 的口不参与就绪 / 终止 / 对齐,
    /// 入队走 cap-1 drop-old(只留最新反馈)。长度恒 = 输入口数(无 back-edge 则全 false)。
    input_is_back_edge: Vec<bool>,
    /// 源节点(0 输入口)自报「已产完」。置位后 readiness 不再放行、节点可关流终止。
    source_done: AtomicBool,
    /// 每个输入口的**时间戳边界**:保证「不会再有时间戳 < bound 的包到来」。
    /// 这是多输入口对齐的依据 —— 只有确知某口不会再来更早的包,
    /// 才能安全地在当前最小时间戳上组一次 Process。
    input_bounds: Vec<Mutex<Timestamp>>,
}

// 安全性:Node 内每个 UnsafeCell<Context> 槽只在被「认领」(从 free_slots 取出而未归还)
// 期间被访问,认领与归还都在 sched 锁下进行,故同一槽任一时刻只有一个访问者。
unsafe impl Sync for Node {}

impl Node {
    /// 取某个 context 槽的可变引用。
    ///
    /// # Safety
    /// 调用者必须**独占持有该槽**(通过在锁下从 `free_slots` 取出而尚未归还),
    /// 或处于尚未开始调度的阶段(build/start/close,此时 in_flight==0)。
    #[allow(clippy::mut_from_ref)]
    unsafe fn ctx_slot(&self, slot: usize) -> &mut Context {
        &mut *self.ctxs[slot].get()
    }

    fn queue_len(&self, port: usize) -> usize {
        self.input_queues[port]
            .lock()
            .expect("queue lock poisoned")
            .len()
    }
    fn bound(&self, port: usize) -> Timestamp {
        *self.input_bounds[port].lock().expect("bound lock poisoned")
    }

    /// 把某口的时间戳边界向前推进(只增不减)。
    fn advance_bound(&self, port: usize, to: Timestamp) {
        let mut b = self.input_bounds[port].lock().expect("bound lock poisoned");
        if to > *b {
            *b = to;
        }
    }

    fn front_ts(&self, port: usize) -> Option<Timestamp> {
        self.input_queues[port]
            .lock()
            .expect("queue lock poisoned")
            .front()
            .map(|p| p.timestamp())
    }

    /// 源节点:没有输入口,由内核自行产出(见 docs/design.md §7.4)。
    fn is_source(&self) -> bool {
        self.input_queues.is_empty()
    }

    /// 正向(非 back-edge)输入口下标。back-edge 是反馈寄存器,不参与就绪 / 终止 / 对齐判定 ——
    /// **核心不变式:back-edge 口永不触发 readiness**,故反馈包不会自激无限重跑。
    fn forward_ports(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.input_queues.len()).filter(move |&i| !self.input_is_back_edge[i])
    }

    /// 就绪判定。返回本次的**触发计划**(处理时间戳 + 参与的输入口);`None` 表示还不能跑。
    ///
    /// `Sync` 采用时间戳对齐:
    ///   * `min_packet` = 各非空口队首时间戳的最小值;
    ///   * `min_bound`  = 各空口边界的最小值(空口若还会来包,最早也不早于其边界);
    ///   * 仅当 `min_bound > min_packet`,才确知「没有任何口还会送来更早的包」,
    ///     于是可以在 `min_packet` 上安全地组一次 Process。
    ///
    /// 这条判据是多输入口正确性的核心:没有它,Zip 之类算子会把不同时刻的数据配到一起。
    fn readiness(&self) -> Option<Ready> {
        let n = self.input_queues.len();
        if n == 0 {
            // 源节点:无输入口。未自报完成即「可产出」;ts 占位(try_claim 用 seq 覆盖成单调时间戳)。
            return (!self.source_done.load(Ordering::SeqCst)).then_some(Ready {
                ts: Timestamp::unset(),
                ports: None,
                batch: None,
            });
        }
        match &self.policy {
            InputPolicy::Immediate => {
                // 不做对齐:任一**正向**口有数据就跑(back-edge 口不触发)
                let mut min = Timestamp::done();
                let mut any = false;
                for i in self.forward_ports() {
                    if let Some(ts) = self.front_ts(i) {
                        any = true;
                        min = min.min(ts);
                    }
                }
                any.then_some(Ready {
                    ts: min,
                    ports: None,
                    batch: None,
                })
            }
            InputPolicy::Sync | InputPolicy::FixedSize { .. } => {
                self.sync_align(self.forward_ports()).map(|ts| Ready {
                    ts,
                    ports: None,
                    batch: None,
                })
            }
            InputPolicy::SyncSet { sets } => {
                // 每组独立对齐;取**最早就绪**的那组,本次只带该组的口。
                let mut best: Option<Ready> = None;
                for set in sets {
                    if let Some(ts) = self.sync_align(set.iter().copied()) {
                        if best.as_ref().is_none_or(|b| ts < b.ts) {
                            best = Some(Ready {
                                ts,
                                ports: Some(set.clone()),
                                batch: None,
                            });
                        }
                    }
                }
                best
            }
            InputPolicy::Batch { size } => self.batch_readiness(*size),
        }
    }

    /// `batch` 策略的就绪判定:把 `sync` 的对齐**连续跑 `size` 轮**(只偷看、不弹出),
    /// 算出每个正向口本次该取的包数。
    ///
    /// **为什么不是「各口各自攒够 `size` 个」**:那会把 0 号口的第 k 个与 1 号口的第 k 个
    /// 配成一对,而它们未必是同一帧 —— 图像批与掩码批就此错位,且**不会报任何错**。
    /// 本项目不接受静默的错误行为,故一批 = `size` 个**对齐元组**,对齐规则与 `sync` 完全同源。
    ///
    /// 不足一批时**只有所有正向口都已关闭**才把余量刷出(不可能再来数据了);否则继续等 ——
    /// 提前交付就是过早切批,那也是一种静默的语义偏差。
    ///
    /// 锁:每口**只拿一次**队列锁,把前 `size` 个时间戳快照出来,之后纯内存模拟
    /// (ADR #36)。前缀在此期间稳定 —— 只有 `try_claim` 会 pop 且全程持 `sched`
    /// (ADR #30),别的线程只往队尾 push。每口游标每轮至多前进 1、共 `size` 轮,
    /// 故快照 `size` 个足够。
    fn batch_readiness(&self, size: usize) -> Option<Ready> {
        let ports: Vec<usize> = self.forward_ports().collect();
        if ports.is_empty() || size == 0 {
            return None;
        }

        let ts_prefix: Vec<Vec<Timestamp>> = ports
            .iter()
            .map(|&p| {
                let q = self.input_queues[p].lock().expect("queue lock poisoned");
                q.iter().take(size).map(|pk| pk.timestamp()).collect()
            })
            .collect();

        let mut take = vec![0usize; ports.len()];
        let mut first_ts: Option<Timestamp> = None;
        let mut last_ts = Timestamp::unset();
        let mut rounds = 0usize;

        while rounds < size {
            // 一轮对齐,逻辑同 `sync_align`:游标处有包的口贡献 min_packet,
            // 没包的口贡献它的 bound —— bound 不够大就说明还可能来更早的包,不能定这一轮。
            let mut min_packet = Timestamp::done();
            let mut min_bound = Timestamp::done();
            for (pi, &p) in ports.iter().enumerate() {
                match ts_prefix[pi].get(take[pi]) {
                    Some(&ts) => min_packet = min_packet.min(ts),
                    None => min_bound = min_bound.min(self.bound(p)),
                }
            }
            if min_packet == Timestamp::done() || min_bound <= min_packet {
                break;
            }
            for (pi, _) in ports.iter().enumerate() {
                if ts_prefix[pi].get(take[pi]) == Some(&min_packet) {
                    take[pi] += 1;
                }
            }
            first_ts.get_or_insert(min_packet);
            last_ts = min_packet;
            rounds += 1;
        }

        if rounds == 0 {
            return None;
        }
        if rounds < size
            && !ports
                .iter()
                .all(|&p| self.input_closed[p].load(Ordering::SeqCst))
        {
            return None; // 还会有数据 → 继续攒,别过早切批
        }

        Some(Ready {
            ts: first_ts.unwrap_or_else(Timestamp::unset),
            ports: None,
            batch: Some(BatchPlan {
                take: ports.into_iter().zip(take).collect(),
                last_ts,
            }),
        })
    }

    /// 在给定的一组输入口上做 sync 对齐,返回对齐到的时间戳(逻辑同 §7.2 的 min_bound>min_packet)。
    fn sync_align(&self, ports: impl Iterator<Item = usize>) -> Option<Timestamp> {
        let mut min_packet = Timestamp::done();
        let mut min_bound = Timestamp::done();
        for i in ports {
            match self.front_ts(i) {
                Some(ts) => min_packet = min_packet.min(ts),
                None => min_bound = min_bound.min(self.bound(i)),
            }
        }
        if min_packet == Timestamp::done() {
            return None; // 没有任何数据
        }
        if min_bound > min_packet {
            Some(min_packet)
        } else {
            None // 某空口还可能送来 <= min_packet 的包,必须再等
        }
    }

    fn all_inputs_closed_and_drained(&self) -> bool {
        if self.is_source() {
            // 源节点没有输入口;只有内核自报完成才算「排空」(否则 (0..0).all() 空真会开图即关)。
            return self.source_done.load(Ordering::SeqCst);
        }
        // 只看正向口:back-edge 口是反馈寄存器,不参与终止判定 —— 否则 A→B→A 里两节点
        // 互等对方关闭,谁也关不了(终止死锁)。节点靠正向输入排空即可关闭,级联绕环拆解。
        self.forward_ports()
            .all(|i| self.input_closed[i].load(Ordering::SeqCst) && self.queue_len(i) == 0)
    }
}

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
    /// 命名线程池,按 YAML 的 executors 顺序
    executors: Vec<ThreadPool>,
    /// 已投递到线程池、尚未跑完的任务数。用于「是否空闲」与终止判定。
    in_flight: AtomicUsize,
    /// 有任何进展时唤醒阻塞中的宿主线程(取到输出、节点关闭、出错、主线程任务入队)
    activity: (Mutex<Activity>, Condvar),
    /// 暂停调度(调试/限速)。已在执行的算子不受影响。
    paused: AtomicBool,
    /// 因下游无损输入队列已满而保留 staging 的节点。下游每次出队后重试这些刷新。
    blocked_flush_nodes: Mutex<BTreeSet<NodeId>>,
    side_packets: Mutex<BTreeMap<String, Packet>>,
    /// 各算子在 GetContract 里声明的必需 side packet:(名字, 声明它的节点)
    required_side_packets: Vec<(String, String)>,
    /// 计时基准。`Instant` 无法放进原子,故节点统计里存「相对本基准的微秒」。
    epoch: Instant,
    /// 是否为每次算子回调计时。建图时由 `config.stats_timing` 与 `watchdog_ms` 定下,
    /// 之后不变(故是普通 bool,不必原子)。见 `GraphConfig::stats_timing`。
    timing: bool,
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
        // 引擎自带的默认 Rust 算子(PassThrough 等)在此自动注册一次 —— 宿主无需调用。
        crate::builtin::register_defaults();
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

    pub fn add_poller(&self, port: &str) -> Result<Poller> {
        self.add_poller_inner(port, None)
    }

    pub fn add_poller_with_options(&self, port: &str, options: PollerOptions) -> Result<Poller> {
        if options.capacity == 0 {
            return Err(Error::InvalidArg(
                "poller capacity must be at least 1".into(),
            ));
        }
        if options.overflow == PollerOverflow::Latest && options.capacity != 1 {
            return Err(Error::InvalidArg(
                "poller overflow=latest requires capacity 1".into(),
            ));
        }
        self.add_poller_inner(port, Some(options))
    }

    fn add_poller_inner(&self, port: &str, options: Option<PollerOptions>) -> Result<Poller> {
        let st = self.state();
        if st != State::Initialized {
            return Err(Error::State(format!(
                "add_poller must be called before start (current state {st:?})"
            )));
        }
        let edge =
            *self.inner.output_by_name.get(port).ok_or_else(|| {
                Error::NotFound(format!("graph output port `{port}` does not exist"))
            })?;
        let inner = Arc::new(PollerInner {
            edge,
            edge_name: port.to_string(),
            queue: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            capacity: options.map(|options| options.capacity),
            overflow: options.map_or(PollerOverflow::Block, |options| options.overflow),
            dropped: AtomicU64::new(0),
            active: AtomicBool::new(true),
        });
        self.inner.edges[edge]
            .pollers
            .lock()
            .expect("poller list lock poisoned")
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
        self.inner.resume_blocked_flushes();
        self.inner.finish_all_backpressure_blocks();
        self.inner.notify_activity();
    }

    pub fn wait_done(&self) -> Result<()> {
        self.inner.wait_done(None)
    }

    pub fn wait_done_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.inner
            .wait_done(Some(std::time::Instant::now() + timeout))
    }

    /// 把已结束的图**复位为可再次 `start`** 的状态,**保留已 open 的算子实例** ——
    /// 省掉每会话重建图 + 重跑 `open`(如重新加载模型)的开销。
    ///
    /// 要求图已 `Terminated` 且静止(通常先 `wait_done()`),否则返回 `Error::State`。
    /// 复位后:所有队列 / 统计 / 时间戳状态归零,`side_packets` 与已注册的
    /// poller / observer **保留**(宿主可复用同一 `Poller` 句柄取下一轮输出)。
    pub fn reset(&self) -> Result<()> {
        self.inner.reset()
    }

    pub fn wait_until_idle(&self) -> Result<()> {
        self.inner.wait_until_idle(None)
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

    /// 是否空闲:主线程队列为空且线程池无在飞任务。
    pub fn is_idle(&self) -> bool {
        self.inner.is_idle_pub()
    }

    /// 推模式订阅输出口。回调在派发该包的线程上执行(可能是线程池线程)。
    ///
    /// 必须在 `start` 之前注册,否则会漏掉已产出的包。
    pub fn observe<F>(&self, port: &str, f: F) -> Result<()>
    where
        F: Fn(&Packet) + Send + Sync + 'static,
    {
        if self.state() != State::Initialized {
            return Err(Error::State(
                "observe must be called before start, otherwise already-produced packets are missed".into(),
            ));
        }
        self.inner.add_observer_fn(port, Arc::new(f))
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
        self.inner.to_dot(false)
    }

    /// 同 [`to_dot`](Self::to_dot),但在每个节点标签上标出运行统计
    /// (处理数 · 平均延迟 · 收/发包数 · 队列峰值 · 错误数),并把填充色换成
    /// **按平均延迟的热力图**(绿=快 → 红=慢)—— 一眼看出瓶颈在哪个节点。
    ///
    /// 可在图运行期间随时调用(统计是原子读的快照),不必等跑完。
    /// 注意:热力图占用了「按执行器上色」那一维,执行器仍以标签里的 `@name` 标出。
    pub fn to_dot_with_stats(&self) -> String {
        self.inner.to_dot(true)
    }

    pub fn node_stats(&self, i: usize) -> Option<NodeStatsSnapshot> {
        self.inner.node_stats(i)
    }

    pub fn input_queue_stats(&self, node: usize, port: usize) -> Option<InputQueueStatsSnapshot> {
        self.inner.input_queue_stats(node, port)
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.inner.active.store(false, Ordering::SeqCst);
        let edge = &self.graph.edges[self.inner.edge];
        edge.pollers
            .lock()
            .expect("poller list lock poisoned")
            .retain(|poller| !Arc::ptr_eq(poller, &self.inner));
        self.inner.clear(&self.graph);
        self.graph.notify_activity();
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

/// 节点单个输入口的队列与背压统计快照。
#[derive(Debug, Clone)]
pub struct InputQueueStatsSnapshot {
    pub node_name: String,
    pub port_name: String,
    pub producer_name: Option<String>,
    pub packet_capacity: Option<usize>,
    pub byte_capacity: Option<u64>,
    pub queued_packets: usize,
    pub queued_bytes: u64,
    pub reserved_packets: usize,
    pub reserved_bytes: u64,
    pub peak_queued_packets: usize,
    pub peak_queued_bytes: u64,
    pub blocked: bool,
    pub blocked_for_us: u64,
    pub block_events: u64,
    pub total_blocked_us: u64,
}

// ---------------------------------------------------------------- 执行

impl GraphInner {
    fn state(&self) -> State {
        *self.state.lock().expect("state lock poisoned")
    }
    fn set_state(&self, s: State) {
        *self.state.lock().expect("state lock poisoned") = s;
    }

    fn start(self: &Arc<Self>) -> Result<()> {
        let st = self.state();
        if st != State::Initialized {
            return Err(Error::State(format!(
                "start can only be called in Initialized (current {st:?})"
            )));
        }

        // 校验算子声明的必需 side packet
        let provided = self.side_packets.lock().expect("side packet lock poisoned");
        for (need, who) in &self.required_side_packets {
            if !provided.contains_key(need) {
                return Err(Error::InvalidArg(format!(
                    "missing required side packet `{need}` -- node `{who}` declared it in GetContract; \
                     inject it with set_side_packet before start"
                )));
            }
        }
        let sp = Arc::new(provided.clone());
        drop(provided);

        // 把 side packets 灌进各节点的**所有** context 槽,然后 open(用槽 0,串行)。
        // reset 重跑时算子实例被保留、`opened` 仍为 true —— 那种情况下**跳过 open**
        // (不重跑 open 正是 reset 省重载模型的价值),只重灌 side packet + 复位槽。
        for (i, node) in self.nodes.iter().enumerate() {
            let already_open = node.sched.lock().expect("scheduler lock poisoned").opened;
            for slot in 0..node.max_in_flight {
                // 安全性:尚未开始调度,所有槽空闲,可独占写入。
                let ctx = unsafe { node.ctx_slot(slot) };
                ctx.side_packets = sp.clone();
                ctx.reset();
                ctx.input_ts = Timestamp::unstarted();
            }
            if already_open {
                continue; // reset 后重跑:算子实例与其 open 状态都保留,不再 open
            }
            let rc = self.call_kernel(i, 0, KernelPhase::Open);
            if rc != 0 {
                let e = unsafe { node.ctx_slot(0) }.take_error(rc);
                self.shared.record_error(e.clone());
                return Err(e);
            }
            node.sched.lock().expect("scheduler lock poisoned").opened = true;
        }

        self.set_state(State::Running);

        // 拉起线程池。必须在 Arc 存在之后:工作线程持 Weak,避免 Arc 环。
        let weak = Arc::downgrade(self);
        for pool in &self.executors {
            pool.start(weak.clone());
        }
        // 源节点(0 输入)无输入触发,须在此显式起调度 —— start 里唯一主动调度的一处。
        // 之后由 finish→schedule_node 自我续产,直到内核 source_done() 或图被 cancel。
        for i in 0..self.nodes.len() {
            if self.nodes[i].is_source() {
                self.schedule_node(i);
            }
        }
        Ok(())
    }

    fn send(&self, edge: EdgeId, pkt: Packet, blocking: bool) -> Result<()> {
        match self.state() {
            State::Running => {}
            State::Draining | State::Terminated => return Err(Error::Closed),
            s => {
                return Err(Error::State(format!(
                    "send requires the graph to be Running (current {s:?}); call start first"
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
                "packets on a graph input port must carry an explicit timestamp (UNSET is invalid)"
                    .into(),
            ));
        }

        // 全局水位:超限时把压力转化成图输入口背压(§7.5)。
        while self.shared.over_watermark() {
            if !blocking {
                return Err(Error::WouldBlock);
            }
            // 长时间背压等待期间图可能被取消/出错,及时退出而不是傻等。
            if self.shared.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if let Some(e) = self.shared.first_error() {
                return Err(e);
            }
            // 先记活动代数,再尝试推进/等待 —— 避免判定与等待之间丢唤醒。
            let before = self.activity_gen();
            if self.pump_step() {
                continue; // 在调用线程上推进了主线程执行器
            }
            // 本线程推不动。若线程池还有在飞任务,就等它们排水(这才是真正的背压);
            // 若全图都空了水位却下不去(如下游无人消费),那是真卡死 —— 报错而非永久阻塞。
            if self.workers_idle() {
                return Err(Error::WouldBlock);
            }
            self.wait_activity_since(before, std::time::Duration::from_millis(100));
        }

        // 时间戳单调性:图输入口强制校验(ADR #23)
        self.check_input_monotonic(edge, &pkt)?;

        // 分发给该边的所有消费者(各自一份引用)与 poller/observer
        self.dispatch(edge, std::slice::from_ref(&pkt)); // 单包不必为它分配 Vec
        self.schedule_consumers(edge);
        Ok(())
    }

    /// 图输入口的时间戳必须严格递增(ADR #23)。
    ///
    /// 参照值单独记录在边上,而不是看队列里剩什么 —— 否则队列一排空,
    /// 回退甚至重复的时间戳就能悄悄混进来,下游行为随之变得难以解释。
    fn check_input_monotonic(&self, edge: EdgeId, pkt: &Packet) -> Result<()> {
        let e = &self.edges[edge];
        let mut last = e.last_sent.lock().expect("timestamp lock poisoned");
        if *last != Timestamp::unset() && pkt.timestamp() <= *last {
            return Err(Error::InvalidArg(format!(
                "graph input port `{}` timestamps must be strictly increasing: previous {}, this one {}",
                e.name,
                *last,
                pkt.timestamp()
            )));
        }
        *last = pkt.timestamp();
        Ok(())
    }

    /// 把一批包投递到边的消费者与订阅者。
    /// 把一批包投递到边的每个消费者队列。**只读 `packets`**(逐个 `clone` 引用计数),
    /// 故取切片而非 `Vec` —— 让调用方保留缓冲的所有权与容量。
    fn dispatch(&self, edge_id: EdgeId, packets: &[Packet]) {
        let edge = &self.edges[edge_id];

        // 订阅者(poller / observer)各自独立一份
        {
            let pollers = edge
                .pollers
                .lock()
                .expect("poller list lock poisoned")
                .clone();
            let mut any = false;
            for p in &pollers {
                for pkt in packets {
                    any |= p.push(self, pkt.clone());
                }
            }
            if any {
                self.notify_activity();
            }
        }
        {
            // 快照订阅者后**释放锁再回调** —— 回调是宿主代码(可能慢、可能回调进引擎),
            // 持锁调用会造成争用甚至重入死锁(observer 若又触达同一条边的 observers 锁)。
            // observer 只增不删,快照是安全的。
            let observers: Vec<Observer> = {
                let guard = edge.observers.lock().expect("observer list lock poisoned");
                if guard.is_empty() {
                    Vec::new()
                } else {
                    guard.clone()
                }
            };
            for o in &observers {
                for pkt in packets {
                    match o {
                        Observer::C { cb, user } => {
                            let ffi = crate::ffi::borrow_packet(pkt);
                            unsafe { cb(*user, ffi) };
                        }
                        Observer::Rust(f) => f(pkt),
                    }
                }
            }
        }

        // 内部消费者:每个输入口一份(仅克隆引用计数)
        for &(node, port) in &edge.consumers {
            let cap = if self.nodes[node].input_is_back_edge[port] {
                Some(1) // 反馈寄存器:cap-1 drop-old,只留最新一包
            } else {
                match &self.nodes[node].policy {
                    InputPolicy::FixedSize { capacity } => Some(*capacity),
                    _ => None,
                }
            };
            let mut dropped = 0u64;
            let mut q = self.nodes[node].input_queues[port]
                .lock()
                .expect("queue lock poisoned");
            for pkt in packets {
                // fixed_size:满则丢最旧的。这是**有意的有损**策略,且不阻塞上游,
                // 故与「内部边不背压」不冲突,而是其配套的内存约束手段。
                if let Some(cap) = cap {
                    while q.len() >= cap {
                        if let Some(old) = q.pop_front() {
                            self.nodes[node].input_queue_bytes[port]
                                .fetch_sub(old.byte_size(), Ordering::SeqCst);
                            self.shared.on_dequeue(old.byte_size());
                            dropped += 1;
                        } else {
                            break;
                        }
                    }
                }
                self.shared.on_enqueue(pkt.byte_size());
                self.nodes[node].input_queue_bytes[port]
                    .fetch_add(pkt.byte_size(), Ordering::SeqCst);
                q.push_back(pkt.clone());
            }
            let depth = q.len();
            let queued_bytes = self.nodes[node].input_queue_bytes[port].load(Ordering::SeqCst);
            self.nodes[node].input_queue_stats[port]
                .peak_packets
                .fetch_max(depth, Ordering::Relaxed);
            self.nodes[node].input_queue_stats[port]
                .peak_bytes
                .fetch_max(queued_bytes, Ordering::Relaxed);
            // 高水位:depth 本就为软限告警算好了,这里顺手 fetch_max —— 定位积压节点。
            self.nodes[node]
                .stats
                .peak_queue_depth
                .fetch_max(depth, Ordering::Relaxed);
            drop(q);
            // 入队后,该口不会再来 <= 最后这个包时间戳的数据
            if let Some(last) = packets.last() {
                self.nodes[node].advance_bound(port, last.timestamp().next_allowed_in_stream());
            }
            if dropped > 0 {
                self.note_dropped(edge_id, dropped);
            }
            self.warn_if_over_soft_limit(edge_id, depth);
        }
    }

    /// 记录丢包。**绝不静默**:首次丢弃打 WARN,之后按指数退避,避免日志洪水。
    fn note_dropped(&self, edge_id: EdgeId, n: u64) {
        let e = &self.edges[edge_id];
        let before = e.dropped.fetch_add(n, Ordering::Relaxed); // 纯计数器
        let after = before + n;
        if before == 0 || after.is_power_of_two() {
            runtime::log_warn(&format!(
                "edge `{}` has dropped {} packets total due to the fixed_size policy (consumer can't keep up; observe with dropped_count)",
                e.name, after
            ));
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
                "edge `{}` has {} packets backlogged (soft limit {}); consumer may not be keeping up",
                self.edges[edge_id].name, depth, limit
            ));
        }
    }

    fn schedule_consumers(&self, edge: EdgeId) {
        let consumers: Vec<NodeId> = self.edges[edge].consumers.iter().map(|&(n, _)| n).collect();
        for n in consumers {
            self.schedule_node(n);
        }
    }

    /// 把一个已认领的调用派给节点所属的执行器。
    /// 与 `try_claim` 1:1 配对(每次成功认领派一个任务)。
    fn dispatch_task(&self, n: NodeId) {
        match self.nodes[n].executor {
            None => {
                self.main_queue
                    .lock()
                    .expect("main queue lock poisoned")
                    .push_back(n);
                self.notify_activity();
            }
            Some(i) => {
                // 先记在飞、再投递 —— 反了会出现「已在跑但计数为 0」的空窗,
                // 使 is_idle 误判为空闲、阻塞接口提前返回。
                self.in_flight.fetch_add(1, Ordering::SeqCst);
                if !self.executors[i].submit(n) {
                    // 池已关停(仅发生在拆图时):撤销全局计数。该次认领残留在 ready 里,
                    // 但拆图路径不依赖精确排空(GraphInner::drop 兜底关流),不会死锁。
                    self.in_flight.fetch_sub(1, Ordering::SeqCst);
                }
                self.notify_activity();
            }
        }
    }

    /// 线程池工作线程的入口。
    pub fn run_node_on_worker(&self, n: NodeId) {
        self.run_node(n);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if self.shared.is_cancelled() || self.shared.has_error() {
            self.resume_blocked_flushes();
            self.finish_all_backpressure_blocks();
        }
        self.notify_activity();
    }

    /// 执行器空闲 = 主线程队列为空且线程池里没有在飞任务。
    fn workers_idle(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0
            && self
                .main_queue
                .lock()
                .expect("main queue lock poisoned")
                .is_empty()
    }

    /// 逻辑空闲还要求没有因内部容量不足而保留的待刷新 staging。
    fn is_idle(&self) -> bool {
        self.workers_idle()
            && self
                .blocked_flush_nodes
                .lock()
                .expect("blocked flush lock poisoned")
                .is_empty()
    }

    /// 任何进展都要通知:取到输出、节点关闭、出错、任务入队/完成。
    /// 否则阻塞中的宿主线程会白等到超时。
    fn notify_activity(&self) {
        let (m, cv) = &self.activity;
        let mut a = m.lock().unwrap_or_else(|e| e.into_inner());
        a.gen = a.gen.wrapping_add(1);
        // 代数**必须**递增(防丢唤醒的本体);但没人在等时就别去做那次 futex 唤醒。
        let wake = a.waiters > 0;
        drop(a);
        if wake {
            cv.notify_all();
        }
    }

    /// 读取当前活动代数。**必须在判断 is_idle/is_done 之前读取**,再据此 `wait_activity_since`,
    /// 否则会丢唤醒:若在「判断非空闲」与「开始等待」之间任务恰好全部完成,
    /// 等待会一直睡到超时(那 55ms 的假慢就是这么来的)。
    fn activity_gen(&self) -> u64 {
        self.activity
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .gen
    }

    /// 等到活动代数不等于 `before`(即有新进展)或超时。
    fn wait_activity_since(&self, before: u64, timeout: std::time::Duration) {
        let (m, cv) = &self.activity;
        let mut a = m.lock().unwrap_or_else(|e| e.into_inner());
        if a.gen != before {
            return; // 已有进展,不必等(也就不必登记为等待者)
        }
        // 在**持锁**期间登记:notifier 要么看见它(会 wake),要么还没拿到锁
        // (那它递增的 gen 会让下面的谓词立刻为假)。见 `Activity` 的说明。
        a.waiters += 1;
        let (mut guard, _res) = cv
            .wait_timeout_while(a, timeout, |x| x.gen == before)
            .unwrap_or_else(|e| e.into_inner());
        guard.waiters -= 1;
        drop(guard);
    }

    /// 认领一次调用:在锁下**原子地**取一个 context 槽、按对齐时间戳弹入输入、分配序号,
    /// 放进 `ready` 待执行。成功返回 true —— 调用方应随即 `dispatch_task` 派一个任务。
    ///
    /// readiness 与弹包必须在同一把锁下完成,否则两个并发认领会取到同一时间戳的包。
    fn try_claim(&self, n: NodeId) -> bool {
        if self.shared.has_error()
            || self.shared.is_cancelled()
            || self.paused.load(Ordering::SeqCst)
        {
            return false;
        }
        let node = &self.nodes[n];
        let mut s = node.sched.lock().expect("scheduler lock poisoned");
        if !s.opened || s.close_started {
            return false;
        }
        if s.blocked_flush.is_some() {
            return false; // 先把已完成调用的 staging 刷出去,再认领新输入
        }
        if s.in_flight >= node.max_in_flight {
            return false; // 达容量上限;释放槽后由 finish→schedule_node 重扫
        }
        // 就绪判定(会短暂锁 input_queues / input_bounds,锁序 sched→queue/bound 一致)
        let Some(ready) = node.readiness() else {
            return false;
        };
        let ts = ready.ts;
        let slot = s
            .free_slots
            .pop()
            .expect("a free slot must exist when in_flight < max");
        let seq = s.next_seq;
        s.next_seq += 1;
        s.in_flight += 1;
        s.ready.push_back((slot, seq));

        // 仍持 sched 锁弹入输入 —— 保证 readiness+pop 原子。该槽此刻独占(刚从 free 取出)。
        let ctx = unsafe { node.ctx_slot(slot) };
        ctx.reset();
        // 批处理:按就绪期算好的计划,给每个正向口整批弹包。各口取数可以不同
        // (对齐语义,见 `batch_readiness`)。input_ts = 本批末尾的对齐时间戳,下游单调。
        if let Some(plan) = &ready.batch {
            for &(port, count) in &plan.take {
                let remaining = {
                    let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                    for _ in 0..count {
                        let Some(p) = q.pop_front() else { break };
                        node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                        self.shared.on_dequeue(p.byte_size());
                        node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                        ctx.input_batches[port].push(p);
                    }
                    q.len() // 顺手读,省一次同一把锁的再获取(ADR #36)
                };
                // 每个参与口都推进到末尾时间戳之后 —— 与 sync 一致:即便某口本批一个包
                // 都没取到,也要告诉下游「本口不会再有 <= last_ts 的数据」。对齐保证了
                // 各口 <= last_ts 的包都已被消费(每轮取的是全局最小)。
                node.advance_bound(port, plan.last_ts.next_allowed_in_stream());
                ctx.inputs_done[port] =
                    node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
            }
            // 反馈口:多输入口的 batch 才使 batch + back_edges 成为可能(单口时它凑不出
            // 「至少一个正向口 + 一个反馈口」)。语义与其它策略一致 —— 每次触发读一次最新值,
            // 不参与对齐、不推进 bound。
            for port in 0..node.input_queues.len() {
                if !node.input_is_back_edge[port] {
                    continue;
                }
                let (popped, remaining) = {
                    let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                    let p = q.pop_front();
                    (p, q.len())
                };
                if let Some(p) = popped {
                    node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                    self.shared.on_dequeue(p.byte_size());
                    node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                    ctx.inputs[port] = Some(p);
                }
                ctx.inputs_done[port] =
                    node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
            }
            ctx.input_ts = plan.last_ts;
            return true;
        }
        for port in 0..node.input_queues.len() {
            if node.input_is_back_edge[port] {
                // 反馈寄存器:取最新一包(队列 cap-1),不参与 ts 对齐、不推进 bound。
                // 首拍(尚无反馈)队列为空 → ctx.inputs[port] = None,内核看到空反馈,自处理。
                let (popped, remaining) = {
                    let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                    let p = q.pop_front();
                    (p, q.len())
                };
                if let Some(p) = popped {
                    node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                    self.shared.on_dequeue(p.byte_size());
                    node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                    ctx.inputs[port] = Some(p);
                }
                ctx.inputs_done[port] =
                    node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
                continue;
            }
            // 只处理「参与本次触发」的口(SyncSet:就绪组;其余策略:全部口)。
            // 非参与口原样不动:不弹包、不推进 bound —— 它的包(可能属别的组)留给下次。
            let participates = ready.ports.as_ref().is_none_or(|set| set.contains(&port));
            if !participates {
                continue;
            }
            // 只取时间戳恰好等于 ts 的包;某口在该时刻没有数据是合法的(算子看到空包),
            // 这正是时间戳对齐的语义 —— 若无条件每口弹一个,就会把不同时刻的数据配到一起。
            // 一次临界区办三件事:读队首 ts、按需弹包、读剩余长度。
            // 原先 `front_ts` / `pop_front` / `queue_len` 各拿一次**同一把**队列锁(每口 3 次)。
            // 安全性:全程持 `sched`,而只有 `try_claim` 会 pop(ADR #30 pop-at-claim),
            // 别的线程只 push(追加尾部、不动队首)—— 故队首稳定。
            // 只取时间戳恰好等于 ts 的包;某口在该时刻没有数据是合法的(算子看到空包),
            // 这正是时间戳对齐的语义 —— 若无条件每口弹一个,就会把不同时刻的数据配到一起。
            let (popped, remaining) = {
                let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                let hit = q.front().map(|p| p.timestamp()) == Some(ts);
                let p = if hit { q.pop_front() } else { None };
                (p, q.len())
            };
            if let Some(p) = popped {
                node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                self.shared.on_dequeue(p.byte_size());
                node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                ctx.inputs[port] = Some(p);
            }
            node.advance_bound(port, ts.next_allowed_in_stream());
            // `remaining` 与 pop 同一临界区内读得。这不改语义:`inputs_done` 还要求
            // `input_closed`,而关流后不再有 push,长度已稳定。
            ctx.inputs_done[port] =
                node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
        }
        // 源节点无输入包,用认领序号当单调时间戳(auto-emit 继承 → 下游单调,复用 seq 重排)。
        ctx.input_ts = if node.is_source() {
            Timestamp(seq as i64)
        } else {
            ts
        };
        true
    }

    /// 尽力填满容量:反复认领并派任务,直到无法再认领。
    /// `max_in_flight == 1` 时每轮至多派一个,与串行行为一致。
    fn schedule_node(&self, n: NodeId) {
        while self.try_claim(n) {
            self.dispatch_task(n);
            // 本次认领已从某些内部输入队列弹包,可能为上游腾出了容量。
            self.resume_blocked_flushes();
        }
    }

    /// 跑一个主线程任务。返回是否真的跑了。
    ///
    /// ⚠ 必须先把 pop 的结果落到局部变量:在 edition 2021 里,`if let` 表达式中的
    /// 临时值(此处是 MutexGuard)会存活到整个 if-let 块结束 —— 那样 `run_node`
    /// 内部再去 `main_queue.lock()` 就自锁死。这也是 R2 锁序规则的实例。
    fn run_one_main_task(&self) -> bool {
        let next = self
            .main_queue
            .lock()
            .expect("main queue lock poisoned")
            .pop_front();
        match next {
            Some(n) => {
                self.run_node(n);
                true
            }
            None => false,
        }
    }

    /// 执行一步:跑一个主线程任务,或推进关流。返回是否真的做了事。
    fn pump_step(&self) -> bool {
        self.run_one_main_task() || self.try_advance_closing()
    }

    fn run_node(&self, n: NodeId) {
        let node = &self.nodes[n];
        // 取出一个待执行的调用(认领时已把输入弹进对应槽)。
        let inv = {
            node.sched
                .lock()
                .expect("scheduler lock poisoned")
                .ready
                .pop_front()
        };
        let Some((slot, seq)) = inv else {
            // 认领与派任务 1:1,理论上不会为空;稳妥起见走一遍收尾。
            self.finish(n);
            return;
        };

        // 契约类型校验(在本槽上)。类型不符宁可报错,也不让算子按错误类型解读内存。
        let ok = match self.check_input_types(n, slot) {
            Err(e) => self.on_node_error(n, slot, e),
            Ok(()) => {
                let rc = self.call_kernel(n, slot, KernelPhase::Process);
                // 源节点:内核调了 source_done() → 记下,readiness 不再放行、随后关流终止。
                if node.is_source() && unsafe { node.ctx_slot(slot) }.source_done {
                    node.source_done.store(true, Ordering::SeqCst);
                }
                if rc != 0 {
                    let e = unsafe { node.ctx_slot(slot) }.take_error(rc);
                    self.on_node_error(n, slot, e)
                } else if let Err(e) = self.check_output_types(n, slot) {
                    self.on_node_error(n, slot, e)
                } else {
                    node.stats.processed.fetch_add(1, Ordering::Relaxed);
                    true
                }
            }
        };
        self.complete_invocation(n, slot, seq, ok);
    }

    /// 调用完成:按 `seq` 顺序刷新输出并释放槽。保证下游看到的时间戳单调 ——
    /// 即使后面的时间戳先算完,也要等前面的先刷。
    /// 算子失败时按本节点的 [`OnError`] 分流。返回值是给 `complete_invocation` 的 `ok`:
    /// **`true` 表示「走刷新路径」**(而非「成功」)。
    ///
    /// `Skip` 之所以返回 `true`,是因为刷新路径才会**推进下游时间戳边界** ——
    /// staging 已被 `discard_staging` 清空,于是 `flush_one` 落到「无产出」分支,
    /// 自动 `propagate_bound(input_ts + 1)`。这正是 `Filter` 丢包时依赖的同一套机制。
    /// 不推进边界的话下游会永远等这一刻,等于把一帧出错升级成整图卡死。
    fn on_node_error(&self, n: NodeId, slot: usize, e: Error) -> bool {
        let node = &self.nodes[n];
        let before = node.stats.errors.fetch_add(1, Ordering::Relaxed);
        match node.on_error {
            OnError::Abort => {
                self.shared.record_error(e);
                false // 丢弃产出,不刷新;has_error 置位后调度不再放行 → 全图终止
            }
            OnError::Skip => {
                // 有损行为绝不静默:计数(node_stats().errors)+ 打 WARN。
                // 指数退避,避免每帧都错时刷爆日志(与 note_dropped 同法)。
                let after = before + 1;
                if before == 0 || after.is_power_of_two() {
                    runtime::log_warn(&format!(
                        "node `{}`: skipping a failed packet (on_error=skip), {} so far: {}",
                        node.name, after, e
                    ));
                }
                // 清掉这一包可能已写了一半的产出,再走刷新路径(仅为推进边界)。
                unsafe { node.ctx_slot(slot) }.discard_staging();
                true
            }
        }
    }

    fn complete_invocation(&self, n: NodeId, slot: usize, seq: u64, ok: bool) {
        let node = &self.nodes[n];
        // 登记结果;当前无人刷新则由本线程担任刷新者。
        //
        // **快路**:重排缓冲为空、本次恰好就是待刷新序号、且当前无人刷新 —— 直接接手,
        // 不必经 `pending_flush`。`max_in_flight == 1`(默认)时这条恒成立:同一时刻只有
        // 一次调用在飞,`seq` 必然等于 `next_flush_seq`。避免每次调用一次 BTreeMap
        // 插入 + 删除(perf 实测这对增删连带堆分配约占 5~6%)。
        let mut first: Option<(usize, bool)> = None;
        let be_flusher = {
            let mut s = node.sched.lock().expect("scheduler lock poisoned");
            if !s.flushing
                && s.blocked_flush.is_none()
                && s.pending_flush.is_empty()
                && seq == s.next_flush_seq
            {
                s.flushing = true;
                first = Some((slot, ok));
                true
            } else {
                s.pending_flush.insert(seq, (slot, ok));
                if s.flushing {
                    false
                } else {
                    s.flushing = true;
                    true
                }
            }
        };
        if be_flusher {
            self.drive_invocation_flushes(n, first);
        }
        self.finish(n);
    }

    /// 驱动按序刷新。下游容量不足时保留槽与 staging,让出 worker;由下游出队后重试。
    fn drive_invocation_flushes(&self, n: NodeId, mut first: Option<(usize, bool)>) {
        let node = &self.nodes[n];
        loop {
            let item = match first.take() {
                Some(value) => Some(value),
                None => {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    if let Some(BlockedFlush::Invocation { slot, ok }) = sched.blocked_flush.take()
                    {
                        Some((slot, ok))
                    } else {
                        let next = sched.next_flush_seq;
                        match sched.pending_flush.remove(&next) {
                            Some(value) => Some(value),
                            None => {
                                sched.flushing = false;
                                None
                            }
                        }
                    }
                }
            };
            let Some((slot, ok)) = item else {
                self.blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .remove(&n);
                return;
            };

            if ok && !self.shared.is_cancelled() && !self.shared.has_error() {
                match self.flush_staging(n, slot) {
                    Ok(true) => {}
                    Ok(false) => {
                        let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                        sched.blocked_flush = Some(BlockedFlush::Invocation { slot, ok });
                        sched.flushing = false;
                        drop(sched);
                        self.blocked_flush_nodes
                            .lock()
                            .expect("blocked flush lock poisoned")
                            .insert(n);
                        return;
                    }
                    Err(error) => {
                        unsafe { node.ctx_slot(slot) }.discard_staging();
                        self.shared.record_error(error);
                    }
                }
            } else {
                unsafe { node.ctx_slot(slot) }.discard_staging();
            }
            unsafe { node.ctx_slot(slot) }.clear_inputs();
            {
                let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                sched.next_flush_seq += 1;
                sched.in_flight -= 1;
                sched.free_slots.push(slot);
            }
        }
    }

    fn resume_blocked_flushes(&self) {
        if self.shared.is_cancelled() || self.shared.has_error() {
            self.finish_all_backpressure_blocks();
        }
        let blocked: Vec<NodeId> = self
            .blocked_flush_nodes
            .lock()
            .expect("blocked flush lock poisoned")
            .iter()
            .copied()
            .collect();
        for node_id in blocked {
            let blocked = {
                let mut sched = self.nodes[node_id]
                    .sched
                    .lock()
                    .expect("scheduler lock poisoned");
                if sched.flushing || sched.blocked_flush.is_none() {
                    None
                } else {
                    sched.flushing = true;
                    sched.blocked_flush
                }
            };
            match blocked {
                Some(BlockedFlush::Invocation { .. }) => {
                    self.drive_invocation_flushes(node_id, None);
                    self.finish(node_id);
                }
                Some(BlockedFlush::Close) => {
                    self.resume_blocked_close(node_id);
                }
                None => {}
            }
        }
    }

    fn resume_blocked_close(&self, n: NodeId) {
        let node = &self.nodes[n];
        if self.shared.is_cancelled() || self.shared.has_error() {
            unsafe { node.ctx_slot(0) }.discard_staging();
            unsafe { node.ctx_slot(0) }.clear_inputs();
            {
                let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                sched.blocked_flush = None;
                sched.flushing = false;
                sched.closed = true;
            }
            self.blocked_flush_nodes
                .lock()
                .expect("blocked flush lock poisoned")
                .remove(&n);
            for &edge in &node.outputs {
                self.close_edge(edge);
            }
            self.notify_activity();
            return;
        }
        match self.flush_staging(n, 0) {
            Ok(true) => {
                unsafe { node.ctx_slot(0) }.clear_inputs();
                {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    sched.blocked_flush = None;
                    sched.flushing = false;
                    sched.closed = true;
                }
                self.blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .remove(&n);
                for &edge in &node.outputs {
                    self.close_edge(edge);
                }
                self.notify_activity();
            }
            Ok(false) => {
                node.sched.lock().expect("scheduler lock poisoned").flushing = false;
            }
            Err(error) => {
                unsafe { node.ctx_slot(0) }.discard_staging();
                unsafe { node.ctx_slot(0) }.clear_inputs();
                self.shared.record_error(error);
                {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    sched.blocked_flush = None;
                    sched.flushing = false;
                    sched.closed = true;
                }
                self.blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .remove(&n);
                for &edge in &node.outputs {
                    self.close_edge(edge);
                }
                self.notify_activity();
            }
        }
    }

    /// 契约声明的输入类型校验。类型不符宁可报错,也不让算子按错误类型解读内存。
    fn check_input_types(&self, n: NodeId, slot: usize) -> Result<()> {
        let node = &self.nodes[n];
        let ctx = unsafe { node.ctx_slot(slot) };
        for (port, &want) in node.input_types.iter().enumerate() {
            let Some(pkt) = ctx.inputs.get(port).and_then(|s| s.as_ref()) else {
                continue;
            };
            if pkt.is_empty() {
                continue; // 空包(时间戳边界)不参与类型校验
            }
            let got = pkt.type_id();

            // `HOST_OBJECT` 预留未启用(ADR #26)。契约声明它已在建图期拒掉,但包**自己**
            // 带 7 是另一条路(C 侧手填 type_id,或 Rust unsafe `from_foreign`)。这一条必须在
            // `want == 0` 的短路**之前**判 —— 否则声明 `any` 的端口(最常见的情形)恰好
            // 就是漏网的那种,而那正是要堵的洞。
            if got == crate::packet::type_id::HOST_OBJECT {
                return Err(Error::Kernel(format!(
                    "[{}] input port `{}` carries LMFLOW_TYPE_HOST_OBJECT, which is reserved \
                     and not enabled (see ADR #26); use LMFLOW_TYPE_BUFFER for numeric \
                     collections, or LMFLOW_TYPE_STR carrying JSON for arbitrary metadata",
                    node.name,
                    node.in_ports.name(port).unwrap_or("?"),
                )));
            }

            if want == 0 {
                continue; // 未声明类型 = 接受任意
            }
            if got != want {
                // `got == NONE` 是一个**有明确出路**的特例,但出路取决于包是谁造的 ——
                // 按 payload 形态分别给建议,否则会把 Rust API 推给 C/C++ 宿主(或反之)。
                // NONE 的来源不止一个:`Packet::new`(Native)、`from_foreign(.., 0, ..)`、
                // 以及 C ABI 侧 type_id 填 0 的自建包(Foreign)。
                let hint = if got == crate::packet::type_id::NONE {
                    match pkt.payload() {
                        Some(crate::packet::Payload::Foreign(_)) => {
                            " (the packet carries no declared type: its type_id is \
                             LMFLOW_TYPE_NONE, which means \"skip type checking\"; set a real \
                             LMFLOW_TYPE_* on the packet you submit, or declare this port as \
                             any-type)"
                        }
                        // Native = Rust 原生 payload,只可能来自 Rust 宿主
                        _ => {
                            " (the packet carries no declared type because its payload is \
                             Rust-native, e.g. built with `Packet::new`; use \
                             `Packet::from_i64` / `from_f64` / `from_builtin` for built-in \
                             payloads, implement `InteropType` and use `Packet::from_interop` \
                             for a custom type, or use unsafe `Packet::new_interop` only after \
                             manually proving the ABI layout)"
                        }
                    }
                } else {
                    ""
                };
                return Err(Error::Kernel(format!(
                    "[{}] input port `{}` type mismatch: contract declares {}, actual {}{}",
                    node.name,
                    node.in_ports.name(port).unwrap_or("?"),
                    crate::packet::type_name(want),
                    crate::packet::type_name(got),
                    hint,
                )));
            }
        }
        Ok(())
    }

    /// 算子暂存输出的类型校验。必须在离开回调后、派发前统一做,因为 C/C++/Python 的
    /// `emit` ABI 是 `void`:不能依赖算子检查返回值。放在这里也能覆盖源节点、图输出、
    /// `close` 产出以及所有语言的算子。
    fn check_output_types(&self, n: NodeId, slot: usize) -> Result<()> {
        let node = &self.nodes[n];
        let ctx = unsafe { node.ctx_slot(slot) };
        for (port, packets) in ctx.staging.iter().enumerate() {
            let want = node.output_types[port];
            for pkt in packets {
                if pkt.is_empty() {
                    continue;
                }
                let got = pkt.type_id();
                if got == crate::packet::type_id::HOST_OBJECT {
                    return Err(Error::Kernel(format!(
                        "[{}] output port `{}` carries LMFLOW_TYPE_HOST_OBJECT, which is reserved \
                         and not enabled (see ADR #26); use LMFLOW_TYPE_BUFFER for numeric \
                         collections, or LMFLOW_TYPE_STR carrying JSON for arbitrary metadata",
                        node.name,
                        node.out_ports.name(port).unwrap_or("?"),
                    )));
                }
                if want != crate::packet::type_id::NONE && got != want {
                    return Err(Error::Kernel(format!(
                        "[{}] output port `{}` type mismatch: contract declares {}, actual {}",
                        node.name,
                        node.out_ports.name(port).unwrap_or("?"),
                        crate::packet::type_name(want),
                        crate::packet::type_name(got),
                    )));
                }
            }
        }
        Ok(())
    }

    /// 把某个槽暂存区的输出分发到下游(此时不持有任何算子回调栈)。
    fn flush_staging(&self, n: NodeId, slot: usize) -> Result<bool> {
        let node = &self.nodes[n];
        let input_ts = unsafe { node.ctx_slot(slot) }.input_ts;
        let reservations = {
            let ctx = unsafe { node.ctx_slot(slot) };
            let outputs: Vec<(EdgeId, usize, u64, bool)> = node
                .outputs
                .iter()
                .copied()
                .zip(ctx.staging.iter())
                .map(|(edge, packets)| {
                    let mut bytes = 0u64;
                    let mut unmeasurable = false;
                    for packet in packets {
                        let packet_bytes = packet.byte_size();
                        unmeasurable |= !packet.has_measurable_byte_size();
                        bytes = bytes.saturating_add(packet_bytes);
                    }
                    (edge, packets.len(), bytes, unmeasurable)
                })
                .collect();
            let Some(reservations) = self.reserve_internal_capacity(n, &outputs)? else {
                return Ok(false);
            };
            reservations
        };
        // 逐口处理,不再先 `collect` 成一个临时 `Vec<OutputBatch>`(perf 显示那个临时
        // Vec 连带 malloc/free 可观)。仍然**不在调用 `dispatch` 时持有 `&mut Context`** ——
        // 那是本函数原有的安全性质(避免与回调期交出的 `*mut Context` 形成别名),保留。
        for i in 0..node.outputs.len() {
            let edge = node.outputs[i];
            let (mut packets, explicit_bound) = {
                let ctx = unsafe { node.ctx_slot(slot) };
                (
                    std::mem::take(&mut ctx.staging[i]),
                    ctx.next_bounds[i].take(),
                )
            };
            self.flush_one(n, edge, &packets, explicit_bound, input_ts);
            // 归还缓冲:清空后放回 staging,容量得以复用 —— 否则下次产出要重新分配。
            packets.clear();
            unsafe { node.ctx_slot(slot) }.staging[i] = packets;
        }
        self.release_internal_reservations(&reservations);
        // 真正入队后 reservation 已转化为 queue len / bytes；此刻重试其它被挡住的刷新。
        self.resume_blocked_flushes();
        Ok(true)
    }

    fn reserve_internal_capacity(
        &self,
        producer: NodeId,
        outputs: &[(EdgeId, usize, u64, bool)],
    ) -> Result<Option<Vec<InputQueueReservation>>> {
        let mut reservations = Vec::new();
        for &(edge, count, bytes, unmeasurable) in outputs {
            if count == 0 {
                continue;
            }
            for &(consumer, port) in &self.edges[edge].consumers {
                let node = &self.nodes[consumer];
                if node.input_is_back_edge[port]
                    || matches!(node.policy, InputPolicy::FixedSize { .. })
                {
                    continue;
                }
                let packet_capacity = node.input_queue_capacity[port];
                let byte_capacity = node.input_queue_byte_capacity[port];
                if packet_capacity.is_none() && byte_capacity.is_none() {
                    continue;
                }
                if let Some(capacity) = packet_capacity.filter(|&capacity| count > capacity) {
                    self.release_internal_reservations(&reservations);
                    return Err(Error::Kernel(format!(
                        "node `{}` emits a batch of {count} packets to edge `{}`, exceeding consumer \
                         `{}` input port `{}` capacity {capacity}; increase the capacity or emit smaller batches",
                        self.nodes[producer].name,
                        self.edges[edge].name,
                        node.name,
                        node.in_ports.name(port).unwrap_or("?"),
                    )));
                }
                if byte_capacity.is_some() && unmeasurable {
                    self.release_internal_reservations(&reservations);
                    return Err(Error::Kernel(format!(
                        "node `{}` emits an unmeasurable payload to edge `{}`, but consumer `{}` \
                         input port `{}` has a byte capacity; use a builtin payload or register a \
                         fixed-size custom type descriptor",
                        self.nodes[producer].name,
                        self.edges[edge].name,
                        node.name,
                        node.in_ports.name(port).unwrap_or("?"),
                    )));
                }
                if let Some(capacity) = byte_capacity.filter(|&capacity| bytes > capacity) {
                    self.release_internal_reservations(&reservations);
                    return Err(Error::Kernel(format!(
                        "node `{}` emits a batch of {bytes} bytes to edge `{}`, exceeding consumer \
                         `{}` input port `{}` byte capacity {capacity}; increase the capacity or emit smaller batches",
                        self.nodes[producer].name,
                        self.edges[edge].name,
                        node.name,
                        node.in_ports.name(port).unwrap_or("?"),
                    )));
                }
                // queue len 与 reservation 必须在同一把 queue 锁下观察/更新。
                // 否则可能读到旧 len，恰逢另一刷新已入队并释放 reservation，
                // 两个刷新都以为有空位而共同越过容量。
                let queue = node.input_queues[port].lock().expect("queue lock poisoned");
                let queued = queue.len();
                let queued_bytes = node.input_queue_bytes[port].load(Ordering::SeqCst);
                let reserved = node.input_queue_reserved[port].load(Ordering::SeqCst);
                let reserved_bytes = node.input_queue_reserved_bytes[port].load(Ordering::SeqCst);
                let packets_full =
                    packet_capacity.is_some_and(|capacity| queued + reserved + count > capacity);
                let bytes_full = byte_capacity.is_some_and(|capacity| {
                    queued_bytes
                        .saturating_add(reserved_bytes)
                        .saturating_add(bytes)
                        > capacity
                });
                if packets_full || bytes_full {
                    drop(queue);
                    self.mark_input_queue_blocked(consumer, port);
                    self.release_internal_reservations(&reservations);
                    return Ok(None);
                }
                self.finish_input_queue_block(consumer, port);
                node.input_queue_reserved[port].fetch_add(count, Ordering::SeqCst);
                node.input_queue_reserved_bytes[port].fetch_add(bytes, Ordering::SeqCst);
                reservations.push(InputQueueReservation {
                    node: consumer,
                    port,
                    packets: count,
                    bytes,
                });
                drop(queue);
            }
        }
        Ok(Some(reservations))
    }

    fn release_internal_reservations(&self, reservations: &[InputQueueReservation]) {
        for reservation in reservations {
            self.nodes[reservation.node].input_queue_reserved[reservation.port]
                .fetch_sub(reservation.packets, Ordering::SeqCst);
            self.nodes[reservation.node].input_queue_reserved_bytes[reservation.port]
                .fetch_sub(reservation.bytes, Ordering::SeqCst);
        }
    }

    fn epoch_us(&self) -> i64 {
        self.epoch.elapsed().as_micros().min(i64::MAX as u128) as i64
    }

    fn mark_input_queue_blocked(&self, node: NodeId, port: usize) {
        let stats = &self.nodes[node].input_queue_stats[port];
        let since = self.epoch_us().saturating_add(1);
        if stats
            .blocked_since_us
            .compare_exchange(0, since, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            stats.block_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn finish_input_queue_block(&self, node: NodeId, port: usize) {
        let stats = &self.nodes[node].input_queue_stats[port];
        let since = stats.blocked_since_us.swap(0, Ordering::SeqCst);
        if since != 0 {
            let elapsed = self
                .epoch_us()
                .saturating_sub(since.saturating_sub(1))
                .max(0) as u64;
            stats.blocked_total_us.fetch_add(elapsed, Ordering::Relaxed);
        }
    }

    fn finish_all_backpressure_blocks(&self) {
        for node in 0..self.nodes.len() {
            for port in 0..self.nodes[node].input_queues.len() {
                self.finish_input_queue_block(node, port);
            }
        }
    }

    /// `flush_staging` 的单个输出口分支(拆出来只为让上面的循环短一点,逻辑未变)。
    fn flush_one(
        &self,
        n: NodeId,
        edge: EdgeId,
        packets: &[Packet],
        explicit_bound: Option<Timestamp>,
        input_ts: Timestamp,
    ) {
        if !packets.is_empty() {
            self.nodes[n]
                .stats
                .packets_out
                .fetch_add(packets.len() as u64, Ordering::Relaxed);
            self.dispatch(edge, packets);
            self.schedule_consumers(edge);
            return;
        }
        // **没有产出时也必须推进下游边界**,否则下游会永远等这一路。
        // 这是自动的:算子不显式调 SetNextTimestampBound 也不会卡住管线
        // (Filter 这类会丢包的算子因此不必自己操心)。
        let bound = match explicit_bound {
            Some(b) => b,
            None if input_ts.is_allowed_in_stream() => input_ts.next_allowed_in_stream(),
            None => return,
        };
        self.propagate_bound(edge, bound);
    }

    /// 把时间戳边界推给某条边的所有消费者,并重扫其就绪性。
    fn propagate_bound(&self, edge: EdgeId, bound: Timestamp) {
        let consumers: Vec<(NodeId, usize)> = self.edges[edge].consumers.clone();
        for (node, port) in consumers {
            self.nodes[node].advance_bound(port, bound);
            self.schedule_node(node);
        }
    }

    /// 一次调用完成后:尽力再填满容量(并行调度多个 in-flight),并尝试关闭。
    fn finish(&self, n: NodeId) {
        self.schedule_node(n);
        self.maybe_close(n);
    }

    /// 关流推进:所有输入已关且排空 → close 算子 → 关自己的输出边 → 递归下游。
    fn maybe_close(&self, n: NodeId) -> bool {
        let node = &self.nodes[n];
        let force = self.shared.has_error() || self.shared.is_cancelled();

        // 在锁下**认领**关流:并发时(宿主线程与工作线程可能同时到这里)
        // 只有一个线程能置位 close_started,从而保证算子的 Close 只被调用一次。
        // in_flight != 0 表示还有并行调用在跑或等待刷新 —— 必须全部落地才能关。
        {
            let mut s = node.sched.lock().expect("scheduler lock poisoned");
            if s.close_started || s.in_flight != 0 || !s.opened {
                return false;
            }
            if !force && !node.all_inputs_closed_and_drained() {
                return false;
            }
            s.close_started = true;
        }

        // 此刻 in_flight==0,所有槽空闲;Close 是串行的,用槽 0。
        {
            let ctx = unsafe { node.ctx_slot(0) };
            ctx.reset();
            ctx.close_reason = self.shared.close_reason();
            ctx.input_ts = Timestamp::done();
        }
        let rc = self.call_kernel(n, 0, KernelPhase::Close);
        if rc != 0 {
            let ctx = unsafe { node.ctx_slot(0) };
            let e = ctx.take_error(rc);
            ctx.discard_staging();
            self.shared.record_error(e);
        } else if let Err(e) = self.check_output_types(n, 0) {
            unsafe { node.ctx_slot(0) }.discard_staging();
            self.shared.record_error(e);
        } else {
            match self.flush_staging(n, 0) {
                Ok(true) => {}
                Ok(false) => {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    sched.blocked_flush = Some(BlockedFlush::Close);
                    sched.flushing = false;
                    drop(sched);
                    self.blocked_flush_nodes
                        .lock()
                        .expect("blocked flush lock poisoned")
                        .insert(n);
                    return false;
                }
                Err(error) => {
                    unsafe { node.ctx_slot(0) }.discard_staging();
                    self.shared.record_error(error);
                }
            }
        }
        unsafe { node.ctx_slot(0) }.clear_inputs();
        node.sched.lock().expect("scheduler lock poisoned").closed = true;

        for &e in &node.outputs {
            self.close_edge(e);
        }
        self.notify_activity();
        true
    }

    fn close_edge(&self, edge: EdgeId) {
        let e = &self.edges[edge];
        if e.closed.swap(true, Ordering::SeqCst) {
            return; // 已关
        }
        for &(node, port) in &e.consumers {
            self.nodes[node].input_closed[port].store(true, Ordering::SeqCst);
            // 关闭即「永远不会再有数据」,边界直接到 Done,让下游不必再等这一路
            self.nodes[node].advance_bound(port, Timestamp::done());
            // 关流会改变就绪判定(空口不再阻塞对齐),必须重扫
            self.schedule_node(node);
        }
        // 该边的 poller 在队列排空后即视为结束
        for p in e.pollers.lock().expect("poller list lock poisoned").iter() {
            p.closed.store(true, Ordering::SeqCst);
        }
    }

    /// 复位为可再次 `start` 的状态,**保留已 open 的算子实例**(省掉每会话重载模型的
    /// 开销)。字段的「保留 / 复位」分类见 docs/design.md §7.13。
    ///
    /// 前提:图必须**已静止** —— `Terminated` 且 `is_idle()`(没有 worker 还在算子里)。
    /// 否则返回 `Error::State`。宿主通常先 `wait_done()` 再 `reset()`。
    ///
    /// **不碰线程池**:worker 随图存活、此刻都 park 在 condvar 上、`stop` 仍为 false,
    /// 下一轮 `start` 直接复用(见 executor.rs 模块头);shutdown+join 只发生在 Drop。
    fn reset(&self) -> Result<()> {
        // 1. 校验静止。in_flight==0 且 main_queue 空 ⇒ 没有 worker 在 run_node 中途,
        //    故下面所有「无并发」的复位都成立(与 Drop / start 用同一条静止依据)。
        {
            let st = *self.state.lock().expect("state lock poisoned");
            if st != State::Terminated || !self.is_idle() {
                return Err(Error::State(
                    "reset requires the graph to be Terminated and idle; call wait_done() first"
                        .into(),
                ));
            }
        }

        // 2. 清 GraphShared:先清 error/cancelled,否则下一轮 start 的 try_claim 会被旧
        //    has_error 挡回(mod.rs try_claim 首判)。
        self.shared.reset_run_state();
        self.blocked_flush_nodes
            .lock()
            .expect("blocked flush lock poisoned")
            .clear();

        // 3. 逐 Edge 复位。last_sent 必须回 unset() —— 否则单调性校验会拒掉下一轮
        //    从图输入口发的第一个包(时间戳通常又从小开始)。
        for e in &self.edges {
            e.closed.store(false, Ordering::SeqCst);
            e.dropped.store(0, Ordering::Relaxed);
            *e.last_sent.lock().expect("last_sent lock poisoned") = Timestamp::unset();
            // poller / observer 是宿主持有、engine 存 Arc —— **保留**列表,只复位内容,
            // 让宿主复用同一个 Poller 句柄再取下一轮输出。
            for pl in e.pollers.lock().expect("poller list lock poisoned").iter() {
                pl.clear(self);
                pl.closed.store(false, Ordering::SeqCst);
                pl.dropped.store(0, Ordering::Relaxed);
            }
        }

        // 4. 逐 Node 复位。
        for node in &self.nodes {
            // sched 整体重建(一把覆盖 next_seq / free_slots / ready / pending_flush 等全部
            // 运行态,不会漏),再单独把 opened 置回 true —— **保留 open 是 reset 的价值**。
            {
                let mut sc = node.sched.lock().expect("scheduler lock poisoned");
                let opened = sc.opened;
                *sc = NodeSched::new(node.max_in_flight);
                sc.opened = opened;
            }
            node.stats.reset();
            for q in &node.input_queues {
                q.lock().expect("queue lock poisoned").clear();
            }
            for bytes in &node.input_queue_bytes {
                bytes.store(0, Ordering::SeqCst);
            }
            for reserved in &node.input_queue_reserved {
                reserved.store(0, Ordering::SeqCst);
            }
            for reserved in &node.input_queue_reserved_bytes {
                reserved.store(0, Ordering::SeqCst);
            }
            for stats in &node.input_queue_stats {
                stats.reset();
            }
            for c in &node.input_closed {
                c.store(false, Ordering::SeqCst);
            }
            // input_bounds 必须回 pre_stream()(不是上一轮 close 推到的 done())——
            // 否则 readiness/对齐会认为每个空口「已到流尾」,语义崩坏。
            for b in &node.input_bounds {
                *b.lock().expect("bound lock poisoned") = Timestamp::pre_stream();
            }
            node.source_done.store(false, Ordering::SeqCst);
            *node.last_fire.lock().expect("last_fire lock poisoned") = None;
            // 逐槽复位 Context:此刻 in_flight==0,与 start/Drop 同为「独占相」,无并发。
            for slot in 0..node.ctxs.len() {
                unsafe { node.ctx_slot(slot) }.reset();
            }
        }

        // 5. GraphInner 顶层。side_packets 保留(下一轮 start 会自动 clone 进各 ctx)。
        //    epoch 不动:它只是 started_us 的诊断基准,running_for_us 本就是近似值。
        self.main_queue
            .lock()
            .expect("main queue lock poisoned")
            .clear();
        self.in_flight.store(0, Ordering::SeqCst);
        {
            let mut a = self.activity.0.lock().unwrap_or_else(|e| e.into_inner());
            a.waiters = 0;
        }
        self.paused.store(false, Ordering::SeqCst);

        // 6. 最后置 state —— 前面的清理对「下一次 start」全部可见后,才对外表现为可 start。
        *self.state.lock().expect("state lock poisoned") = State::Initialized;
        Ok(())
    }

    fn set_state_draining_if_all_inputs_closed(&self) {
        let all = self.graph_inputs.iter().all(|&e| self.edges[e].is_closed());
        if all {
            let mut st = self.state.lock().expect("state lock poisoned");
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
            .all(|n| n.sched.lock().expect("scheduler lock poisoned").closed)
    }

    /// 等待图跑完。`deadline` 为 `None` 表示不限时。
    ///
    /// 期间会**借用宿主线程**执行主线程任务(默认执行器,§7.9),
    /// 同时等待线程池里的任务完成。
    fn wait_done(&self, deadline: Option<std::time::Instant>) -> Result<()> {
        loop {
            // 先把能自己干的干完
            while self.pump_step() {}
            if self.all_nodes_closed() {
                break;
            }
            // 在判断是否空闲**之前**捕获活动代数,再据此等待 —— 否则会丢唤醒。
            let before = self.activity_gen();
            if self.workers_idle() {
                self.resume_blocked_flushes();
                let blocked: Vec<&str> = self
                    .blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .iter()
                    .map(|&node| self.nodes[node].name.as_str())
                    .collect();
                if !blocked.is_empty() {
                    return Err(Error::Kernel(format!(
                        "wait_done: internal backpressure cannot make progress; blocked producers: [{}]. \
                         increase the input queue packet/byte capacity or inspect downstream alignment",
                        blocked.join(", ")
                    )));
                }
                // 空闲且未全关:再推一轮关流
                if self.try_advance_closing() {
                    continue;
                }
                // try_advance_closing 把最后一个节点关掉并置 Terminated 时会返回 false
                // (它不把“到达终态”算作推进)。此时图其实已跑完 —— 常见触发是:工作线程
                // 在本轮 all_nodes_closed() 判定与这里之间关掉了最后一个节点。必须重判,
                // 否则会把已完成的图误报成“卡住”(症状:未能关闭的节点列表为空 [])。
                if self.all_nodes_closed() {
                    break;
                }
                // 推不动了。这时**不能返回 Ok** —— 图并没有跑完。
                // 区分两种成因,给出可操作的报错而不是静默成功或永久挂住:
                let inputs_open: Vec<&str> = self
                    .graph_inputs
                    .iter()
                    .filter(|&&e| !self.edges[e].is_closed())
                    .map(|&e| self.edges[e].name.as_str())
                    .collect();
                if !inputs_open.is_empty() {
                    return Err(Error::State(format!(
                        "wait_done: graph input ports [{}] still open, the graph won't finish on its own -- \
                         call close_input/close_all_inputs first",
                        inputs_open.join(", ")
                    )));
                }
                let stuck: Vec<&str> = (0..self.nodes.len())
                    .filter(|&n| {
                        !self.nodes[n]
                            .sched
                            .lock()
                            .expect("scheduler lock poisoned")
                            .closed
                    })
                    .map(|n| self.nodes[n].name.as_str())
                    .collect();
                return Err(Error::Kernel(format!(
                    "wait_done: all inputs closed but the graph is still idle, nodes not closed: [{}]. \
                     usually some kernel's output/close condition is unmet (use dump to inspect queue backlog)",
                    stuck.join(", ")
                )));
            }
            // 线程池还在跑:等它有进展(相对刚才捕获的 before)
            match self.remaining(deadline) {
                Some(d) => {
                    self.wait_activity_since(before, d);
                }
                None => return Err(Error::Timeout),
            }
        }
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

    /// 等到在途任务都处理完(但不结束图)。
    fn wait_until_idle(&self, deadline: Option<std::time::Instant>) -> Result<()> {
        loop {
            while self.run_one_main_task() {}
            self.resume_blocked_flushes();
            // 判断空闲**之前**捕获代数,防止丢唤醒(见 activity_gen)。
            let before = self.activity_gen();
            if self.is_idle() {
                break;
            }
            if self.workers_idle() {
                let blocked: Vec<&str> = self
                    .blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .iter()
                    .map(|&node| self.nodes[node].name.as_str())
                    .collect();
                return Err(Error::Kernel(format!(
                    "wait_until_idle: internal backpressure cannot make progress; blocked producers: [{}]",
                    blocked.join(", ")
                )));
            }
            match self.remaining(deadline) {
                Some(d) => {
                    self.wait_activity_since(before, d);
                }
                None => return Err(Error::Timeout),
            }
        }
        match self.shared.first_error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 距 deadline 还剩多久;`None` 表示已超时。无 deadline 时返回一个固定的
    /// 轮询上限,以免因通知丢失而永久挂住。
    fn remaining(&self, deadline: Option<std::time::Instant>) -> Option<std::time::Duration> {
        const POLL_CAP: std::time::Duration = std::time::Duration::from_millis(50);
        match deadline {
            None => Some(POLL_CAP),
            Some(d) => {
                let now = std::time::Instant::now();
                if now >= d {
                    None
                } else {
                    Some((d - now).min(POLL_CAP))
                }
            }
        }
    }

    /// 调用算子回调(在指定 context 槽上)。**调用期间不持有任何引擎锁**(R1),
    /// 并记录耗时以便定位卡死。可被并发调用(不同槽),故 `process` 必须可重入。
    fn call_kernel(&self, n: NodeId, slot: usize, phase: KernelPhase) -> i32 {
        let node = &self.nodes[n];
        // 直接交出 UnsafeCell 内部指针:不构造 Rust 引用,故与回调内
        // 从该指针造出的 `&mut Context` 不冲突(该槽此刻由本调用独占持有)。
        let ctx_ptr = node.ctxs[slot].get() as *mut c_void;
        // 记账全走原子:改造前这里每次调用要拿 2 次 running_timing 锁 + 1 次 stats 锁
        // (再加 run_node 里的 processed 一次)。R1 要求「调算子时不持任何引擎锁」——
        // 原子天然满足,也顺带把 4 对 mutex 从每包热路径上去掉了。
        // 计时可关(见 `GraphConfig::stats_timing`):`Instant::now()` + 末尾的 `elapsed()`
        // 是**每次 process 两次**时钟读,本机约 43 ns、占单跳派发成本约 15%。
        // 一次时钟读两用:既作本次耗时起点,也作「本节点开始在跑」的时刻。
        let started = if self.timing {
            Some(Instant::now())
        } else {
            None
        };
        if node.stats.in_flight.fetch_add(1, Ordering::Relaxed) == 0 {
            if let Some(t) = started {
                let since_epoch = t.saturating_duration_since(self.epoch).as_micros() as i64;
                node.stats.started_us.store(since_epoch, Ordering::Relaxed);
            }
        }

        // 源节点定速(见 `NodeConfig::rate`):在**调算子之前**、于本节点的池线程里
        // sleep 到点,保证相邻两次 process 至少隔 min_period。R1 未被破坏 —— 此刻不持任何
        // 引擎锁(last_fire 那把在 sleep 前已释放)。只有源节点会设 min_period;它本就串行
        // 自续产(finish→schedule_node→try_claim 紧循环),故这里节流就等于给整条产出限速。
        if matches!(phase, KernelPhase::Process) {
            if let Some(period) = node.min_period {
                let now = Instant::now();
                let wait = {
                    let mut last = node.last_fire.lock().expect("last_fire lock poisoned");
                    let w = match *last {
                        Some(prev) => period.checked_sub(now.duration_since(prev)),
                        None => None, // 首次不等
                    };
                    // 预支下一次的基准:按「本次实际放行时刻」记,避免累积漂移。
                    *last = Some(now + w.unwrap_or_default());
                    w
                }; // 锁在此释放,再 sleep —— 不持锁阻塞
                if let Some(w) = wait {
                    std::thread::sleep(w);
                }
            }
        }

        // 安全性:ctx_ptr 来自本槽的 UnsafeCell,该槽此刻独占。
        let rc = unsafe {
            match phase {
                KernelPhase::Open => node.kernel.open(ctx_ptr),
                KernelPhase::Process => node.kernel.process(ctx_ptr),
                KernelPhase::Close => node.kernel.close(ctx_ptr),
            }
        };

        // 归零时**不清** started_us:读侧按 in_flight > 0 判断是否在跑,
        // 故无需清零,也就不存在「清零」与「新一次开始」互相覆盖的竞争。
        node.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
        let Some(t0) = started else { return rc }; // 计时关闭:统计与 watchdog 都不适用
        let us = t0.elapsed().as_micros() as i64;
        if matches!(phase, KernelPhase::Process) {
            node.stats.total_us.fetch_add(us, Ordering::Relaxed);
            node.stats.max_us.fetch_max(us, Ordering::Relaxed);
        }
        let wd = self.shared.config.watchdog_ms;
        if wd > 0 && us as u64 > wd * 1000 {
            runtime::log_warn(&format!(
                "node `{}`: one {:?} took {} ms, exceeding watchdog {} ms",
                node.name,
                phase,
                us / 1000,
                wd
            ));
        }
        rc
    }

    pub fn add_observer(
        &self,
        port: &str,
        cb: unsafe extern "C" fn(*mut c_void, crate::ffi::LMFlowPacket),
        user: *mut c_void,
    ) -> Result<()> {
        let edge = *self
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("graph output port `{port}` does not exist")))?;
        self.edges[edge]
            .observers
            .lock()
            .expect("observer list lock poisoned")
            .push(Observer::C { cb, user });
        Ok(())
    }

    /// Rust 宿主的推模式订阅。回调在**派发该包的线程**上执行(可能是池线程),
    /// 因此必须 `Send + Sync`;回调内不得再调 graph 的生命周期接口。
    pub fn add_observer_fn(&self, port: &str, f: Arc<dyn Fn(&Packet) + Send + Sync>) -> Result<()> {
        let edge = *self
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("graph output port `{port}` does not exist")))?;
        self.edges[edge]
            .observers
            .lock()
            .expect("observer list lock poisoned")
            .push(Observer::Rust(f));
        Ok(())
    }

    // ffi 层需要的只读访问
    pub fn nodes_len(&self) -> usize {
        self.nodes.len()
    }
    pub fn node_name_at(&self, i: usize) -> Option<&str> {
        self.nodes.get(i).map(|n| n.name.as_str())
    }
    pub fn node_input_ports_len(&self, node: usize) -> usize {
        self.nodes.get(node).map_or(0, |value| value.in_ports.len())
    }
    pub fn node_input_port_name_at(&self, node: usize, port: usize) -> Option<&str> {
        self.nodes.get(node)?.in_ports.name(port)
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
    pub fn start_pub(self: &Arc<Self>) -> Result<()> {
        self.start()
    }
    pub fn wait_done_pub(&self) -> Result<()> {
        self.wait_done(None)
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

    // ---- Poller 需要的内部能力 ----
    pub(crate) fn remaining_for_poller(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> Option<std::time::Duration> {
        self.remaining(deadline)
    }
    pub(crate) fn wait_activity_since_pub(&self, before: u64, d: std::time::Duration) {
        self.wait_activity_since(before, d);
    }
    pub(crate) fn activity_gen_pub(&self) -> u64 {
        self.activity_gen()
    }
    pub(crate) fn is_idle_pub(&self) -> bool {
        self.is_idle()
    }

    // ---- 暂停 / 恢复 ----
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// 恢复调度,并重扫一遍就绪节点 —— 暂停期间到达的包否则会一直躺着。
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        for n in 0..self.nodes.len() {
            self.schedule_node(n);
        }
        self.notify_activity();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn executor_names(&self) -> Vec<&str> {
        self.executors.iter().map(|p| p.name()).collect()
    }

    pub(crate) fn shutdown_executors_pub(&self) {
        self.shutdown_executors();
    }

    /// 关停所有线程池并 join。**必须在动节点之前做**。
    fn shutdown_executors(&self) {
        for p in &self.executors {
            p.shutdown();
        }
    }
}

impl Drop for GraphInner {
    /// 兜底关流:图被直接丢弃(没走 wait_done)时,已 open 的算子仍必须收到 Close,
    /// 否则算子里申请的资源(文件、连接、GPU 上下文)不会被释放。
    fn drop(&mut self) {
        // 先关停线程池并 join:否则工作线程可能触碰正在析构的节点。
        self.shutdown_executors();

        for n in 0..self.nodes.len() {
            let need_close = {
                let s = self.nodes[n].sched.lock().expect("scheduler lock poisoned");
                s.opened && !s.close_started
            };
            if !need_close {
                continue;
            }
            {
                // 安全性:线程池已 join,此刻只有 drop 这一条执行流,独占成立。用槽 0。
                let ctx = unsafe { self.nodes[n].ctx_slot(0) };
                ctx.reset();
                ctx.close_reason = self.shared.close_reason();
                ctx.input_ts = Timestamp::done();
            }
            let rc = self.call_kernel(n, 0, KernelPhase::Close);
            if rc != 0 {
                runtime::log_warn(&format!(
                    "node `{}`: close returned {rc} during graph destruction (ignored)",
                    self.nodes[n].name
                ));
            }
            unsafe { self.nodes[n].ctx_slot(0) }.clear_inputs();
            self.nodes[n]
                .sched
                .lock()
                .expect("scheduler lock poisoned")
                .closed = true;
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum KernelPhase {
    Open,
    Process,
    Close,
}
