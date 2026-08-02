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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::config::GraphConfig;
use crate::context::{Context, Options};
use crate::executor::ThreadPool;
use crate::kernel::{Contract, KernelInstance, PortTable};
use crate::packet::Packet;
use crate::runtime::{self, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

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
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
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
    queue: Mutex<VecDeque<Packet>>,
    closed: AtomicBool,
}

impl PollerInner {
    fn push(&self, p: Packet) {
        self.queue
            .lock()
            .expect("poller lock poisoned")
            .push_back(p);
    }
    fn pop(&self) -> Option<Packet> {
        self.queue.lock().expect("poller lock poisoned").pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.lock().expect("poller lock poisoned").is_empty()
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
            if let Some(p) = self.inner.pop() {
                return Ok(Some(p));
            }
            if self.inner.closed.load(Ordering::SeqCst) || self.graph.shared.has_error() {
                // 先把队列排干再宣告结束
                return Ok(self.inner.pop());
            }
            // 有主线程任务就顺手跑掉(默认执行器就是宿主线程)
            if self.graph.pump_step() {
                continue;
            }
            // 在判断空闲**之前**捕获活动代数(防丢唤醒,见 GraphInner::activity_gen)。
            let before = self.graph.activity_gen_pub();
            if self.graph.is_idle_pub() {
                // 主线程与线程池都空了 —— 不会再有新输出
                return Ok(self.inner.pop());
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
        self.inner.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
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
    /// 是否已有线程在做刷新 —— 保证刷新按序、串行(否则并发刷新会打乱下游顺序)。
    flushing: bool,
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
            flushing: false,
        }
    }
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
    /// `None` = 宿主主线程(默认执行器,ADR #16);`Some(i)` = executors[i] 线程池
    executor: Option<usize>,
    policy: InputPolicy,
    input_types: Vec<u64>,
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
    stats: Mutex<NodeStats>,
    /// 每个输入口一条独立队列(见模块头注释)
    input_queues: Vec<Mutex<VecDeque<Packet>>>,
    input_closed: Vec<AtomicBool>,
    /// 源节点(0 输入口)自报「已产完」。置位后 readiness 不再放行、节点可关流终止。
    source_done: AtomicBool,
    /// 每个输入口的**时间戳边界**:保证「不会再有时间戳 < bound 的包到来」。
    /// 这是多输入口对齐的依据 —— 只有确知某口不会再来更早的包,
    /// 才能安全地在当前最小时间戳上组一次 Process。
    input_bounds: Vec<Mutex<Timestamp>>,
    /// 正在执行算子回调的计时:(并发数, 最早开始时刻)—— 让「卡死」可定位。
    running_timing: Mutex<(usize, Option<Instant>)>,
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
            });
        }
        match &self.policy {
            InputPolicy::Immediate => {
                // 不做对齐:任一口有数据就跑
                let mut min = Timestamp::done();
                let mut any = false;
                for i in 0..n {
                    if let Some(ts) = self.front_ts(i) {
                        any = true;
                        min = min.min(ts);
                    }
                }
                any.then_some(Ready {
                    ts: min,
                    ports: None,
                })
            }
            InputPolicy::Sync | InputPolicy::FixedSize { .. } => {
                self.sync_align(0..n).map(|ts| Ready { ts, ports: None })
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
                            });
                        }
                    }
                }
                best
            }
        }
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
    /// 命名线程池,按 YAML 的 executors 顺序
    executors: Vec<ThreadPool>,
    /// 已投递到线程池、尚未跑完的任务数。用于「是否空闲」与终止判定。
    in_flight: AtomicUsize,
    /// 有任何进展时唤醒阻塞中的宿主线程(取到输出、节点关闭、出错、主线程任务入队)
    activity: (Mutex<u64>, Condvar),
    /// 暂停调度(调试/限速)。已在执行的算子不受影响。
    paused: AtomicBool,
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
        self.inner
            .side_packets
            .lock()
            .expect("side packet lock poisoned")
            .insert(name.to_string(), pkt);
        Ok(())
    }

    pub fn add_poller(&self, port: &str) -> Result<Poller> {
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
            queue: Mutex::new(VecDeque::new()),
            closed: AtomicBool::new(false),
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
    }

    pub fn wait_done(&self) -> Result<()> {
        self.inner.wait_done(None)
    }

    pub fn wait_done_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.inner
            .wait_done(Some(std::time::Instant::now() + timeout))
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

    pub fn node_stats(&self, i: usize) -> Option<NodeStatsSnapshot> {
        self.inner.node_stats(i)
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

        // ---- 校验 4:成环 ----
        check_acyclic(&cfg, &edges)?;

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
                stats: Mutex::new(NodeStats::default()),
                input_queues: (0..ins.len())
                    .map(|_| Mutex::new(VecDeque::new()))
                    .collect(),
                input_closed: (0..ins.len()).map(|_| AtomicBool::new(false)).collect(),
                source_done: AtomicBool::new(false),
                input_bounds: (0..ins.len())
                    .map(|_| Mutex::new(Timestamp::pre_stream()))
                    .collect(),
                running_timing: Mutex::new((0, None)),
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
                            "topology cycle: node `{}` -> ... -> `{}` (back-edges are not supported in this version; a cycle would deadlock)",
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
        for (i, node) in self.nodes.iter().enumerate() {
            for slot in 0..node.max_in_flight {
                // 安全性:尚未开始调度,所有槽空闲,可独占写入。
                let ctx = unsafe { node.ctx_slot(slot) };
                ctx.side_packets = sp.clone();
                ctx.reset();
                ctx.input_ts = Timestamp::unstarted();
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
            if self.is_idle() {
                return Err(Error::WouldBlock);
            }
            self.wait_activity_since(before, std::time::Duration::from_millis(100));
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
    fn dispatch(&self, edge_id: EdgeId, packets: Vec<Packet>) {
        let edge = &self.edges[edge_id];

        // 订阅者(poller / observer)各自独立一份
        {
            let pollers = edge.pollers.lock().expect("poller list lock poisoned");
            let mut any = false;
            for p in pollers.iter() {
                for pkt in &packets {
                    p.push(pkt.clone());
                    any = true;
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
                for pkt in &packets {
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
            let cap = match &self.nodes[node].policy {
                InputPolicy::FixedSize { capacity } => Some(*capacity),
                _ => None,
            };
            let mut dropped = 0u64;
            let mut q = self.nodes[node].input_queues[port]
                .lock()
                .expect("queue lock poisoned");
            for pkt in &packets {
                // fixed_size:满则丢最旧的。这是**有意的有损**策略,且不阻塞上游,
                // 故与「内部边不背压」不冲突,而是其配套的内存约束手段。
                if let Some(cap) = cap {
                    while q.len() >= cap {
                        if let Some(old) = q.pop_front() {
                            self.shared.on_dequeue(old.byte_size());
                            dropped += 1;
                        } else {
                            break;
                        }
                    }
                }
                self.shared.on_enqueue(pkt.byte_size());
                q.push_back(pkt.clone());
            }
            let depth = q.len();
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
        let before = e.dropped.fetch_add(n, Ordering::SeqCst);
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
        self.notify_activity();
    }

    /// 空闲 = 主线程队列为空 **且** 线程池里没有在飞任务。
    fn is_idle(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0
            && self
                .main_queue
                .lock()
                .expect("main queue lock poisoned")
                .is_empty()
    }

    /// 任何进展都要通知:取到输出、节点关闭、出错、任务入队/完成。
    /// 否则阻塞中的宿主线程会白等到超时。
    fn notify_activity(&self) {
        let (m, cv) = &self.activity;
        let mut gen = m.lock().unwrap_or_else(|e| e.into_inner());
        *gen = gen.wrapping_add(1);
        drop(gen);
        cv.notify_all();
    }

    /// 读取当前活动代数。**必须在判断 is_idle/is_done 之前读取**,再据此 `wait_activity_since`,
    /// 否则会丢唤醒:若在「判断非空闲」与「开始等待」之间任务恰好全部完成,
    /// 等待会一直睡到超时(那 55ms 的假慢就是这么来的)。
    fn activity_gen(&self) -> u64 {
        *self.activity.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 等到活动代数不等于 `before`(即有新进展)或超时。
    fn wait_activity_since(&self, before: u64, timeout: std::time::Duration) {
        let (m, cv) = &self.activity;
        let gen = m.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _res) = cv
            .wait_timeout_while(gen, timeout, |g| *g == before)
            .unwrap_or_else(|e| e.into_inner());
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
        for port in 0..node.input_queues.len() {
            // 只处理「参与本次触发」的口(SyncSet:就绪组;其余策略:全部口)。
            // 非参与口原样不动:不弹包、不推进 bound —— 它的包(可能属别的组)留给下次。
            let participates = ready.ports.as_ref().is_none_or(|set| set.contains(&port));
            if !participates {
                continue;
            }
            // 只取时间戳恰好等于 ts 的包;某口在该时刻没有数据是合法的(算子看到空包),
            // 这正是时间戳对齐的语义 —— 若无条件每口弹一个,就会把不同时刻的数据配到一起。
            if node.front_ts(port) == Some(ts) {
                if let Some(p) = node.input_queues[port]
                    .lock()
                    .expect("queue lock poisoned")
                    .pop_front()
                {
                    self.shared.on_dequeue(p.byte_size());
                    ctx.inputs[port] = Some(p);
                }
            }
            node.advance_bound(port, ts.next_allowed_in_stream());
            ctx.inputs_done[port] =
                node.input_closed[port].load(Ordering::SeqCst) && node.queue_len(port) == 0;
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
            Err(e) => {
                node.stats.lock().expect("stats lock poisoned").errors += 1;
                self.shared.record_error(e);
                false
            }
            Ok(()) => {
                let rc = self.call_kernel(n, slot, KernelPhase::Process);
                // 源节点:内核调了 source_done() → 记下,readiness 不再放行、随后关流终止。
                if node.is_source() && unsafe { node.ctx_slot(slot) }.source_done {
                    node.source_done.store(true, Ordering::SeqCst);
                }
                if rc != 0 {
                    let e = unsafe { node.ctx_slot(slot) }.take_error(rc);
                    node.stats.lock().expect("stats lock poisoned").errors += 1;
                    self.shared.record_error(e);
                    false
                } else {
                    node.stats.lock().expect("stats lock poisoned").processed += 1;
                    true
                }
            }
        };
        self.complete_invocation(n, slot, seq, ok);
    }

    /// 调用完成:按 `seq` 顺序刷新输出并释放槽。保证下游看到的时间戳单调 ——
    /// 即使后面的时间戳先算完,也要等前面的先刷。
    fn complete_invocation(&self, n: NodeId, slot: usize, seq: u64, ok: bool) {
        let node = &self.nodes[n];
        // 登记结果;当前无人刷新则由本线程担任刷新者。
        let be_flusher = {
            let mut s = node.sched.lock().expect("scheduler lock poisoned");
            s.pending_flush.insert(seq, (slot, ok));
            if s.flushing {
                false
            } else {
                s.flushing = true;
                true
            }
        };
        if be_flusher {
            loop {
                // 严格按 next_flush_seq 取;取不到就在同一临界区里让出刷新者身份,避免丢刷新。
                let item = {
                    let mut s = node.sched.lock().expect("scheduler lock poisoned");
                    let next = s.next_flush_seq;
                    match s.pending_flush.remove(&next) {
                        Some(v) => Some(v),
                        None => {
                            s.flushing = false;
                            None
                        }
                    }
                };
                let Some((fslot, fok)) = item else { break };
                // 锁外刷新;因 flushing 独占,刷新严格单线程按序。
                if fok {
                    self.flush_staging(n, fslot);
                } else {
                    unsafe { node.ctx_slot(fslot) }.discard_staging();
                }
                // CoW 卫生:立刻释放本次输入的引用(否则上游 CoW 退化成全量拷贝)。
                unsafe { node.ctx_slot(fslot) }.clear_inputs();
                {
                    let mut s = node.sched.lock().expect("scheduler lock poisoned");
                    s.next_flush_seq += 1;
                    s.in_flight -= 1;
                    s.free_slots.push(fslot);
                }
            }
        }
        self.finish(n);
    }

    /// 契约声明的输入类型校验。类型不符宁可报错,也不让算子按错误类型解读内存。
    fn check_input_types(&self, n: NodeId, slot: usize) -> Result<()> {
        let node = &self.nodes[n];
        let ctx = unsafe { node.ctx_slot(slot) };
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
                    "[{}] input port `{}` type mismatch: contract declares {}, actual {}",
                    node.name,
                    node.in_ports.name(port).unwrap_or("?"),
                    crate::packet::type_name(want),
                    crate::packet::type_name(got),
                )));
            }
        }
        Ok(())
    }

    /// 把某个槽暂存区的输出分发到下游(此时不持有任何算子回调栈)。
    fn flush_staging(&self, n: NodeId, slot: usize) {
        let node = &self.nodes[n];
        let (input_ts, batches): (Timestamp, Vec<OutputBatch>) = {
            let ctx = unsafe { node.ctx_slot(slot) };
            let ts = ctx.input_ts;
            let v = node
                .outputs
                .iter()
                .enumerate()
                .map(|(i, &e)| {
                    (
                        e,
                        std::mem::take(&mut ctx.staging[i]),
                        ctx.next_bounds[i].take(),
                    )
                })
                .collect();
            (ts, v)
        };
        for (edge, packets, explicit_bound) in batches {
            if !packets.is_empty() {
                self.dispatch(edge, packets);
                self.schedule_consumers(edge);
                continue;
            }
            // **没有产出时也必须推进下游边界**,否则下游会永远等这一路。
            // 这是自动的:算子不显式调 SetNextTimestampBound 也不会卡住管线
            // (Filter 这类会丢包的算子因此不必自己操心)。
            let bound = match explicit_bound {
                Some(b) => b,
                None if input_ts.is_allowed_in_stream() => input_ts.next_allowed_in_stream(),
                None => continue,
            };
            self.propagate_bound(edge, bound);
        }
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
        } else {
            self.flush_staging(n, 0);
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
            if self.is_idle() {
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
            // 判断空闲**之前**捕获代数,防止丢唤醒(见 activity_gen)。
            let before = self.activity_gen();
            if self.is_idle() {
                break;
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
        {
            let mut t = node.running_timing.lock().expect("timing lock poisoned");
            if t.0 == 0 {
                t.1 = Some(Instant::now());
            }
            t.0 += 1;
        }
        let started = Instant::now();

        // 安全性:ctx_ptr 来自本槽的 UnsafeCell,该槽此刻独占。
        let rc = unsafe {
            match phase {
                KernelPhase::Open => node.kernel.open(ctx_ptr),
                KernelPhase::Process => node.kernel.process(ctx_ptr),
                KernelPhase::Close => node.kernel.close(ctx_ptr),
            }
        };

        let us = started.elapsed().as_micros() as i64;
        {
            let mut t = node.running_timing.lock().expect("timing lock poisoned");
            t.0 -= 1;
            if t.0 == 0 {
                t.1 = None;
            }
        }
        if matches!(phase, KernelPhase::Process) {
            let mut st = node.stats.lock().expect("stats lock poisoned");
            st.total_us += us;
            if us > st.max_us {
                st.max_us = us;
            }
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

    fn queue_depth(&self, edge: EdgeId) -> usize {
        self.edges[edge]
            .consumers
            .iter()
            .map(|&(n, p)| self.nodes[n].queue_len(p))
            .sum()
    }

    fn node_stats(&self, i: usize) -> Option<NodeStatsSnapshot> {
        let node = self.nodes.get(i)?;
        let st = node.stats.lock().expect("stats lock poisoned");
        let (run_count, earliest) = *node.running_timing.lock().expect("timing lock poisoned");
        let since = if run_count > 0 { earliest } else { None };
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
            let st = self.node_stats(i).expect("node exists");
            let sched = self.nodes[i].sched.lock().expect("scheduler lock poisoned");
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

/// 一个输出口在本次调用中要向下游投递的内容:边 + 包 + 可选的显式时间戳边界。
///
/// 即便没有包也要带上边界:否则下游会永远等这一路(见 `flush_staging`)。
type OutputBatch = (EdgeId, Vec<Packet>, Option<Timestamp>);

#[derive(Debug, Clone, Copy)]
enum KernelPhase {
    Open,
    Process,
    Close,
}
