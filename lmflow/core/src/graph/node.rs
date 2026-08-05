use super::*;

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
    pub(super) fn from_config(
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
pub(super) struct Ready {
    pub(super) ts: Timestamp,
    pub(super) ports: Option<Vec<usize>>,
    /// 仅 `batch` 策略:就绪判定时已算好的取包计划。放在这里而不是认领时重算,
    /// 是为了保住「每口只拿一次队列锁」(ADR #36)—— 判定期已把各口时间戳前缀
    /// 快照过一次,认领期照计划批量弹出即可,不必再逐轮加锁。
    pub(super) batch: Option<BatchPlan>,
}

/// `batch` 策略的认领计划:每个**正向口**本次取多少个包,以及本批末尾的对齐时间戳。
///
/// 各口取数**可以不同** —— 某口在某个对齐时间戳上没有包,该轮就不取它。这与 `sync`
/// 单包时的语义一致(`Context::input_count` 本就是按口计数的),而不是「各口各自数够
/// `size` 个」:后者会把 0 号口的第 k 个与 1 号口的第 k 个配成一对,而它们未必是同一帧,
/// 属于**静默的错误配对**。
pub(super) struct BatchPlan {
    /// (端口号, 取包数)
    pub(super) take: Vec<(usize, usize)>,
    /// 本批最后一轮对齐到的时间戳:用作 `input_ts`,并据此推进各口 bound。
    pub(super) last_ts: Timestamp,
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

/// Graphviz DOT 输出的详细程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotView {
    /// 仅拓扑、执行器与静态容量。
    Topology,
    /// 节点状态与核心吞吐/延迟统计,隐藏逐端口诊断和 Poller 细节。
    Compact,
    /// 完整运行诊断,包括端口队列、背压、Poller 与诊断图例。
    Diagnostics,
}

// ---------------------------------------------------------------- 边

pub struct Edge {
    pub name: String,
    pub producer: Option<NodeId>,
    /// (消费者节点, 它的第几个输入口)
    pub consumers: Vec<(NodeId, usize)>,
    pub is_graph_input: bool,
    pub is_graph_output: bool,
    pub(super) closed: AtomicBool,
    pub(super) dropped: AtomicU64,
    pub(super) watermark_backpressure: BackpressureStats,
    /// 该边上最近一次投递的时间戳。**必须独立记录**,不能拿「队列里还剩的包」当参照 ——
    /// 队列一排空参照就消失了,回退的时间戳就能混进来。
    pub(super) last_sent: Mutex<Timestamp>,
    pub(super) pollers: Mutex<Vec<Arc<PollerInner>>>,
    pub(super) observers: Mutex<Vec<Observer>>,
}

impl Edge {
    pub(super) fn new(name: String) -> Self {
        Self {
            name,
            producer: None,
            consumers: Vec::new(),
            is_graph_input: false,
            is_graph_output: false,
            closed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            watermark_backpressure: BackpressureStats::default(),
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
pub(super) enum Observer {
    C {
        cb: unsafe extern "C" fn(*mut c_void, crate::ffi::LMFlowPacket),
        user: *mut c_void,
    },
    Rust(Arc<dyn Fn(&Packet) + Send + Sync>),
}
// 安全性:user 是宿主的不透明指针,引擎只原样回传。
unsafe impl Send for Observer {}
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
pub(super) struct NodeSched {
    pub(super) opened: bool,
    /// 已「认领关流」—— 在锁下置位,保证并发时只有一个线程会调算子的 Close。
    /// 必须与 `closed` 分开:`closed` 表示 Close 已跑完,终止判定看它。
    pub(super) close_started: bool,
    pub(super) closed: bool,
    /// 已认领但尚未刷新的调用数(= 占用中的槽数)。归零方可关闭。
    pub(super) in_flight: usize,
    /// 可用的 context 槽序号(初始 0..max_in_flight)。
    pub(super) free_slots: Vec<usize>,
    /// 已认领、等待某个 worker 来执行的调用:(slot, seq)。
    pub(super) ready: VecDeque<(usize, u64)>,
    /// 取时间戳时分配的下一个序号。
    pub(super) next_seq: u64,
    /// 下一个可刷新的序号(保证下游时间戳单调)。
    pub(super) next_flush_seq: u64,
    /// 完成但等待按序刷新的调用:seq -> (slot, 是否成功)。
    pub(super) pending_flush: BTreeMap<u64, (usize, bool)>,
    /// 当前按序轮到、但因下游内部输入队列已满而暂缓刷新的槽。
    pub(super) blocked_flush: Option<BlockedFlush>,
    /// 是否已有线程在做刷新 —— 保证刷新按序、串行(否则并发刷新会打乱下游顺序)。
    pub(super) flushing: bool,
    /// Source 本次调用刷新完成后，延迟多久再唤醒。
    pub(super) source_reschedule: Option<std::time::Duration>,
}
impl NodeSched {
    pub(super) fn new(max_in_flight: usize) -> Self {
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
            source_reschedule: None,
        }
    }
}

/// 节点级运行统计。**全原子、无锁** —— 每包每节点都要更新,放 `Mutex` 里就是在热路径上
/// 加锁(改造前每包要拿 4 次锁:计时进/出 + 耗时 + processed)。
///
/// 计数器用 `Relaxed`:它们不参与任何 happens-before 推理,只被读侧当快照看。
/// `max_in_flight > 1` 时同一节点会被多个工作线程并发更新,故必须是多写者安全的。
#[derive(Debug, Default)]
pub(super) struct NodeStats {
    pub(super) processed: AtomicU64,
    pub(super) errors: AtomicU64,
    pub(super) total_us: AtomicI64,
    pub(super) max_us: AtomicI64,
    /// 本节点从输入口取走的包数(在 `try_claim` 弹包处累加)
    pub(super) packets_in: AtomicU64,
    /// 本节点产出并派发下游的包数(在 `flush_staging` 派发处累加)
    pub(super) packets_out: AtomicU64,
    /// 下游入队时观察到的**队列深度峰值**(高水位)—— 定位积压点
    pub(super) peak_queue_depth: AtomicUsize,
    /// 正在执行算子回调的并发数(> 0 即「在跑」)
    pub(super) in_flight: AtomicUsize,
    /// 最近一次 `in_flight` 0→1 跃变的时刻(相对 [`GraphInner::epoch`] 的微秒)。
    /// **归零时不清零** —— 读侧一律先看 `in_flight > 0` 再用它,从而避开
    /// 「清零」与「新一次开始」互相覆盖的竞争(那会让诊断值瞬时错乱)。
    pub(super) started_us: AtomicI64,
}

impl NodeStats {
    /// 全字段清零,供图 reset 重跑用。仅在图静止时调用(无并发),但字段是内嵌原子,
    /// 故用 `&self` 逐个 store 即可(不需要 `&mut`)。
    pub(super) fn reset(&self) {
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
    pub(super) fn from_config(s: &str) -> Self {
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
    /// 本节点所属执行器在 `GraphInner::executors` 中的下标。
    pub(super) executor: usize,
    pub(super) policy: InputPolicy,
    pub(super) input_types: Vec<u64>,
    pub(super) output_types: Vec<u64>,
    pub(super) kernel: KernelInstance,
    /// 每次并行 in-flight 调用一个 context 槽(池大小 = max_in_flight)。
    /// 用 UnsafeCell 而非 Mutex:算子回调期间引擎必须交出一个 `*mut Context` 给 C 侧,
    /// 若同时持有 Mutex guard 的 `&mut`,回调里从裸指针再造 `&mut` 就构成别名 UB。
    /// 独占性由「一个槽同一时刻只被一个调用持有」保证(槽在锁下认领/归还)。
    /// 池在 build 后不再增长,故元素地址稳定 —— 交给 C 侧的 `*mut Context` 在
    /// 调用期间始终有效。
    pub(super) ctxs: Vec<UnsafeCell<Context>>,
    pub(super) max_in_flight: usize,
    pub(super) sched: Mutex<NodeSched>,
    /// 全原子,无锁 —— 见 [`NodeStats`]
    pub(super) stats: NodeStats,
    /// 每个输入口一条独立队列(见模块头注释)
    pub(super) input_queues: Vec<Mutex<VecDeque<Packet>>>,
    /// 每个正向输入口的无损包数容量。`None` = 不限。
    pub(super) input_queue_capacity: Vec<Option<usize>>,
    /// 已由上游刷新预留、尚未真正入队的槽数。与 queue len 合计做并发容量判定。
    pub(super) input_queue_reserved: Vec<AtomicUsize>,
    /// 当前各输入队列内 payload 的浅字节数。
    pub(super) input_queue_bytes: Vec<AtomicU64>,
    /// 每个输入口的背压与高水位统计。
    pub(super) input_queue_stats: Vec<InputQueueStats>,
    pub(super) input_closed: Vec<AtomicBool>,
    /// 算子失败时的处理策略(见 [`OnError`])。建图期定下,之后不变。
    pub(super) on_error: OnError,
    /// 源节点定速:相邻两次 `process` 的最小间隔。`None` = 不限速(见 `NodeConfig::rate`)。
    pub(super) min_period: Option<std::time::Duration>,
    /// 上次 `process` 的开始时刻,配合 `min_period` 节流。仅源节点用到 ——
    /// 源本就串行自续产(一个包跑完才排下一个),故一把 Mutex 足够、无竞争压力。
    pub(super) last_fire: Mutex<Option<Instant>>,
    /// 每个输入口是否为 back-edge(反馈寄存器):true 的口不参与就绪 / 终止 / 对齐,
    /// 入队走 cap-1 drop-old(只留最新反馈)。长度恒 = 输入口数(无 back-edge 则全 false)。
    pub(super) input_is_back_edge: Vec<bool>,
    /// 源节点(0 输入口)自报「已产完」。置位后 readiness 不再放行、节点可关流终止。
    pub(super) source_done: AtomicBool,
    /// Source 正在协作式等待下一次唤醒；等待期间不可再次认领。
    pub(super) source_waiting: AtomicBool,
    /// Source 唤醒代次。reset/取消后旧延迟任务会因代次不匹配而失效。
    pub(super) source_wake_generation: AtomicU64,
    /// 每个输入口的**时间戳边界**:保证「不会再有时间戳 < bound 的包到来」。
    /// 这是多输入口对齐的依据 —— 只有确知某口不会再来更早的包,
    /// 才能安全地在当前最小时间戳上组一次 Process。
    pub(super) input_bounds: Vec<Mutex<Timestamp>>,
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
    pub(super) unsafe fn ctx_slot(&self, slot: usize) -> &mut Context {
        &mut *self.ctxs[slot].get()
    }

    pub(super) fn queue_len(&self, port: usize) -> usize {
        self.input_queues[port]
            .lock()
            .expect("queue lock poisoned")
            .len()
    }
    pub(super) fn bound(&self, port: usize) -> Timestamp {
        *self.input_bounds[port].lock().expect("bound lock poisoned")
    }

    /// 把某口的时间戳边界向前推进(只增不减)。
    pub(super) fn advance_bound(&self, port: usize, to: Timestamp) {
        let mut b = self.input_bounds[port].lock().expect("bound lock poisoned");
        if to > *b {
            *b = to;
        }
    }

    pub(super) fn front_ts(&self, port: usize) -> Option<Timestamp> {
        self.input_queues[port]
            .lock()
            .expect("queue lock poisoned")
            .front()
            .map(|p| p.timestamp())
    }

    /// 源节点:没有输入口,由内核自行产出(见 docs/design.md §7.4)。
    pub(super) fn is_source(&self) -> bool {
        self.input_queues.is_empty()
    }

    /// 正向(非 back-edge)输入口下标。back-edge 是反馈寄存器,不参与就绪 / 终止 / 对齐判定 ——
    /// **核心不变式:back-edge 口永不触发 readiness**,故反馈包不会自激无限重跑。
    pub(super) fn forward_ports(&self) -> impl Iterator<Item = usize> + '_ {
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
    pub(super) fn readiness(&self) -> Option<Ready> {
        let n = self.input_queues.len();
        if n == 0 {
            // 源节点:无输入口。未自报完成即「可产出」;ts 占位(try_claim 用 seq 覆盖成单调时间戳)。
            return (!self.source_done.load(Ordering::SeqCst)
                && !self.source_waiting.load(Ordering::SeqCst))
            .then_some(Ready {
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
    pub(super) fn batch_readiness(&self, size: usize) -> Option<Ready> {
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
    pub(super) fn sync_align(&self, ports: impl Iterator<Item = usize>) -> Option<Timestamp> {
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

    pub(super) fn all_inputs_closed_and_drained(&self) -> bool {
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
