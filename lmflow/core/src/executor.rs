//! 执行器:把「就绪节点」派给某个执行器执行。
//!
//! 两种执行器,由 YAML 的 `executors[].type` 选:
//!  * [`ThreadPool`] —— 拥有自己的工作线程,真并发。**默认执行器就是它**。
//!  * [`DelegatingExecutor`] —— 一个线程都不拥有,把任务交还**宿主线程**跑;
//!    宿主进入阻塞接口期间才被抽取执行(见该类型的文档)。
//!
//! 两者对调度器是同一个东西([`Executor`]),`GraphInner::dispatch_task` 不为谁分叉。
//!
//! 零外部依赖(只用 std):任务队列 = `Mutex<VecDeque<NodeId>>` + `Condvar`。
//!
//! 两条关键约定
//!  * 工作线程持 **`Weak<GraphInner>`**。若持强引用会与「GraphInner 拥有执行器」
//!    构成 `Arc` 环,图永远不会被释放。
//!  * `GraphInner::drop` 必须**先关停并 join 线程池**,再去动节点 ——
//!    否则工作线程可能触碰正在析构的节点。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::graph::{GraphInner, NodeId};

#[derive(Clone, Copy)]
enum Task {
    Run(NodeId),
    WakeSource(NodeId, u64),
}

struct DelayedTask {
    deadline: Instant,
    task: Task,
}

#[derive(Default)]
struct QueueState {
    ready: VecDeque<Task>,
    delayed: Vec<DelayedTask>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExecutorStatsSnapshot {
    pub queued: usize,
    pub running: usize,
    pub peak_queued: usize,
    pub completed: u64,
}

#[derive(Default)]
struct ExecutorStats {
    queued: AtomicUsize,
    running: AtomicUsize,
    peak_queued: AtomicUsize,
    completed: AtomicU64,
}

impl ExecutorStats {
    fn enqueued(&self) {
        let queued = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_queued.fetch_max(queued, Ordering::Relaxed);
    }

    fn started(&self) {
        self.queued.fetch_sub(1, Ordering::Relaxed);
        self.running.fetch_add(1, Ordering::Relaxed);
    }

    fn completed(&self) {
        self.running.fetch_sub(1, Ordering::Relaxed);
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    fn dropped(&self, count: usize) {
        let _ = self
            .queued
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
                Some(queued.saturating_sub(count))
            });
    }

    fn snapshot(&self) -> ExecutorStatsSnapshot {
        ExecutorStatsSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            running: self.running.load(Ordering::Relaxed),
            peak_queued: self.peak_queued.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
        }
    }

    fn reset(&self) {
        self.queued.store(0, Ordering::Relaxed);
        self.running.store(0, Ordering::Relaxed);
        self.peak_queued.store(0, Ordering::Relaxed);
        self.completed.store(0, Ordering::Relaxed);
    }
}

struct Shared {
    queue: Mutex<QueueState>,
    cv: Condvar,
    stop: AtomicBool,
    stats: ExecutorStats,
}

impl Shared {
    /// 取一个任务;返回 `None` 表示「已关停且队列排空」,工作线程可以退出。
    fn take(&self) -> Option<Task> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let now = Instant::now();
            while queue
                .delayed
                .first()
                .is_some_and(|delayed| delayed.deadline <= now)
            {
                let delayed = queue.delayed.remove(0);
                queue.ready.push_back(delayed.task);
            }
            if let Some(task) = queue.ready.pop_front() {
                self.stats.started();
                return Some(task);
            }
            if self.stop.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(delayed) = queue.delayed.first() {
                let timeout = delayed.deadline.saturating_duration_since(now);
                let (next, _) = self
                    .cv
                    .wait_timeout(queue, timeout)
                    .unwrap_or_else(|e| e.into_inner());
                queue = next;
            } else {
                queue = self.cv.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}

pub struct ThreadPool {
    name: String,
    num_threads: usize,
    /// CPU 亲和力:worker `i` 绑到 `affinity[i % len]` 号核。空 = 不绑。
    affinity: Vec<usize>,
    /// 实时优先级(SCHED_FIFO,1..=99)。0 = 不动(普通分时)。
    priority: i32,
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// 把**当前线程**绑定到指定 CPU 核(Linux/Android)。绑核是尽力而为的优化 ——
/// 其它平台、或核不存在等失败情形,一律静默降级为「不绑」,绝不影响正确性。
#[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
fn pin_current_thread_to(cpu: usize) {
    // 直接声明 libc(glibc/Bionic)早已链接进来的符号,避免引入 libc crate(本引擎坚持零外部 crate 依赖)。
    extern "C" {
        fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u64) -> i32;
    }
    const NBITS: usize = 1024; // cpu_set_t 位宽(glibc/Bionic 一致)
    if cpu >= NBITS {
        return;
    }
    let mut mask = [0u64; NBITS / 64];
    mask[cpu / 64] |= 1u64 << (cpu % 64);
    // pid=0 表示调用线程本身;失败就算了,绑核只是优化。
    unsafe {
        let _ = sched_setaffinity(0, std::mem::size_of_val(&mask), mask.as_ptr());
    }
}

#[cfg(any(not(any(target_os = "linux", target_os = "android")), miri))]
fn pin_current_thread_to(_cpu: usize) {}

/// 把**当前线程**设为 SCHED_FIFO 实时优先级(Linux/Android)。**尽力而为**:设实时调度需要
/// CAP_SYS_NICE / root,拿不到就静默失败(线程照常以普通分时跑,不影响正确性)。
///
/// 与绑核配合是刻意的:实时线程只在被绑的核上抢占,万一算子死循环也不会拖垮整机。
#[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
fn set_current_thread_rt_priority(prio: i32) {
    extern "C" {
        fn sched_setscheduler(pid: i32, policy: i32, param: *const SchedParam) -> i32;
    }
    // Linux `struct sched_param { int sched_priority; }`。
    #[repr(C)]
    struct SchedParam {
        sched_priority: i32,
    }
    const SCHED_FIFO: i32 = 1;
    let param = SchedParam {
        sched_priority: prio.clamp(1, 99),
    };
    unsafe {
        let _ = sched_setscheduler(0, SCHED_FIFO, &param);
    }
}

#[cfg(miri)]
fn set_current_thread_rt_priority(_prio: i32) {}

/// Darwin(macOS/iOS)没有应用可用的 SCHED_FIFO 式实时优先级 —— Apple 用 **QoS class**
/// 表达线程重要性。把 `priority>0` 映射成「用户在等结果」的高 QoS(推理正合适):
/// 一般用 `USER_INITIATED`,顶格(>=90)才用 `USER_INTERACTIVE`(那档 Apple 留给 UI)。
/// 同样是尽力而为,失败无妨。
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn set_current_thread_rt_priority(prio: i32) {
    extern "C" {
        // libSystem 一直链接着,直接声明,无需 libc crate。
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    const QOS_CLASS_USER_INITIATED: u32 = 0x19;
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    let qos = if prio >= 90 {
        QOS_CLASS_USER_INTERACTIVE
    } else {
        QOS_CLASS_USER_INITIATED
    };
    unsafe {
        let _ = pthread_set_qos_class_self_np(qos, 0);
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn set_current_thread_rt_priority(_prio: i32) {}

impl ThreadPool {
    pub fn new(name: &str, num_threads: usize, affinity: Vec<usize>, priority: i32) -> Self {
        Self {
            name: name.to_string(),
            // 0 视作 1:配了池却一个线程都没有肯定不是本意
            num_threads: num_threads.max(1),
            affinity,
            priority,
            shared: Arc::new(Shared {
                queue: Mutex::new(QueueState::default()),
                cv: Condvar::new(),
                stop: AtomicBool::new(false),
                stats: ExecutorStats::default(),
            }),
            threads: Mutex::new(Vec::new()),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn num_threads(&self) -> usize {
        self.num_threads
    }
    /// CPU 亲和力核列表(空 = 不绑)。仅用于内省 / 可视化。
    pub fn affinity(&self) -> &[usize] {
        &self.affinity
    }
    /// 实时优先级(0 = 普通分时)。仅用于内省 / 可视化。
    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// 拉起工作线程。必须在 `Arc<GraphInner>` 已存在之后调用(需要 `Weak`)。
    pub fn start(&self, graph: Weak<GraphInner>) {
        let mut handles = self.threads.lock().unwrap_or_else(|e| e.into_inner());
        if !handles.is_empty() {
            return; // 已启动
        }
        for i in 0..self.num_threads {
            let shared = self.shared.clone();
            let weak = graph.clone();
            let tname = format!("{}-{}", self.name, i);
            // 绑核:worker i 绑到 affinity[i % len];列表为空则不绑。
            let cpu = if self.affinity.is_empty() {
                None
            } else {
                Some(self.affinity[i % self.affinity.len()])
            };
            let priority = self.priority;
            let h = std::thread::Builder::new()
                .name(tname)
                .spawn(move || {
                    if let Some(c) = cpu {
                        pin_current_thread_to(c);
                    }
                    if priority > 0 {
                        set_current_thread_rt_priority(priority);
                    }
                    worker(shared, weak)
                })
                .expect("failed to create worker thread");
            handles.push(h);
        }
    }

    /// 投递一个就绪节点。关停后返回 `false`,由调用方善后(释放已取得的令牌)。
    pub fn submit(&self, node: NodeId) -> bool {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        // 在**队列锁内**查 stop:与 shutdown 的锁内置位配对。否则关停后仍可能入队,
        // 任务成孤儿留在队里(其 in_flight 永不归零)。
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        queue.ready.push_back(Task::Run(node));
        self.shared.stats.enqueued();
        drop(queue);
        self.shared.cv.notify_one();
        true
    }

    pub fn submit_source_wake(&self, node: NodeId, generation: u64, delay: Duration) -> bool {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        let task = Task::WakeSource(node, generation);
        if delay.is_zero() {
            queue.ready.push_back(task);
        } else {
            let now = Instant::now();
            let mut bounded_delay = delay;
            let deadline = loop {
                if let Some(deadline) = now.checked_add(bounded_delay) {
                    break deadline;
                }
                bounded_delay /= 2;
            };
            let index = queue
                .delayed
                .partition_point(|delayed| delayed.deadline <= deadline);
            queue.delayed.insert(index, DelayedTask { deadline, task });
        }
        self.shared.stats.enqueued();
        drop(queue);
        self.shared.cv.notify_one();
        true
    }

    pub fn pending(&self) -> usize {
        self.shared.stats.queued.load(Ordering::Relaxed)
    }

    pub fn clear_delayed(&self) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        let count = queue.delayed.len();
        queue.delayed.clear();
        self.shared.stats.dropped(count);
        drop(queue);
        self.shared.cv.notify_all();
    }

    pub(crate) fn stats(&self) -> ExecutorStatsSnapshot {
        self.shared.stats.snapshot()
    }

    pub fn reset_stats(&self) {
        self.shared.stats.reset();
    }

    /// 关停并 join。幂等。
    pub fn shutdown(&self) {
        // 置 stop **必须在队列锁内**:take() 是「锁内查 stop → cv.wait(原子释放锁并 park)」。
        // 若在锁外置位 + notify_all,可能恰好落在 worker「查到 stop=false」与「park」之间 ——
        // 那次唤醒无人接收而丢失,worker 永久 park,shutdown 的 join 随之挂死。
        // 这是高并发超订下偶发的死锁根因;锁内置位使 stop 的读写全程受同一把锁保护,窗口消失。
        {
            let _q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            self.shared.stop.store(true, Ordering::SeqCst);
        }
        self.shared.cv.notify_all();
        let handles: Vec<JoinHandle<()>> = {
            let mut h = self.threads.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *h)
        };
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(shared: Arc<Shared>, graph: Weak<GraphInner>) {
    while let Some(task) = shared.take() {
        // upgrade 失败 = 图已销毁。此时丢弃任务是正确的:节点本身已不存在。
        let Some(graph) = graph.upgrade() else { break };
        match task {
            Task::Run(node) => graph.run_node_on_worker(node),
            Task::WakeSource(node, generation) => graph.wake_source_on_worker(node, generation),
        }
        shared.stats.completed();
        graph.executor_task_completed();
    }
}

// ---------------------------------------------------------------- 委托执行器

/// 委托执行器:一个线程都不拥有,把就绪节点**交还宿主线程**跑。
///
/// ⚠ **任务的执行时机** —— 引擎不能凭空占用宿主线程,只能在宿主**进入引擎**时借用它。
/// 所以 `submit` 只是入队,真正执行发生在宿主调用下列阻塞接口期间
/// (由 `GraphInner::run_one_main_task` 抽取):
///
/// ```text
/// lmflow_graph_wait_done / _timeout
/// lmflow_graph_wait_until_idle / _timeout
/// lmflow_poller_next / _timeout
/// lmflow_input_send(阻塞等待空位时)
/// ```
///
/// 推论:若宿主只 `send` 而从不调用上述任一接口,挂在本执行器上的节点**不会推进**
/// (想主动推进而不阻塞,用 `Graph::pump_step`)。反之,这些接口在等待期间一律抽取
/// 并执行委托任务,故不会因此死锁。
///
/// 同一张图的委托任务由一个原子闸门保证**零并发**,多个委托执行器之间轮询抽取,
/// 因而既保持确定顺序,也不会互相饿死。任务跑在实际进入上述接口或调用 `pump_step`
/// 的那个宿主线程上；若 Python 主线程负责推进,Python 算子就在主线程执行且同图内
/// 不会互相争抢 GIL。
///
/// 队列自持(而不是挂在 `GraphInner` 上)是刻意的:这样「宿主线程」在调度器眼里
/// 就只是又一个执行器,派任务的地方无需为它开特例。
pub struct DelegatingExecutor {
    name: String,
    queue: Mutex<VecDeque<NodeId>>,
    stats: ExecutorStats,
}

impl DelegatingExecutor {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            queue: Mutex::new(VecDeque::new()),
            stats: ExecutorStats::default(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 入队待宿主抽取。**恒返回 `true`**:委托执行器没有「已关停」这个状态 ——
    /// 宿主线程不由引擎拉起,也无从 join。拆图时队里的残留由 `GraphInner::drop`
    /// 的兜底关流负责(与线程池 `submit` 失败后的善后同理)。
    pub fn submit(&self, node: NodeId) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(node);
        self.stats.enqueued();
        true
    }

    pub fn pending(&self) -> usize {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 弹一个待办交给宿主线程跑;`None` = 队列空。
    pub fn take(&self) -> Option<NodeId> {
        let node = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        if node.is_some() {
            self.stats.started();
        }
        node
    }

    pub fn complete(&self) {
        self.stats.completed();
    }

    pub(crate) fn stats(&self) -> ExecutorStatsSnapshot {
        self.stats.snapshot()
    }

    /// 清空队列(`reset` 用:上一轮的残留不能带进下一轮)。
    pub fn clear(&self) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let count = queue.len();
        queue.clear();
        self.stats.dropped(count);
    }

    pub fn reset_stats(&self) {
        self.stats.reset();
    }
}

// ---------------------------------------------------------------- 统一抽象

/// 引擎隐式默认执行器的名字。
///
/// 节点不写 `executor` 就归到它。它**完全由引擎持有**:恒是一个按 CPU 核数开线程的
/// 线程池,不绑核、不设实时优先级,YAML 里无从干涉 —— 想控制这些就自己声明一个具名池,
/// 把节点用 `executor:` 指过去。
///
/// 因此这个名字在 `executors` 里是**保留的**,声明即报错:否则一张图里会同时出现
/// 两个 `default`。`executors` 里写的一律是宿主自己的执行器,且必须有名字。
pub const DEFAULT_EXECUTOR_NAME: &str = "default";

/// 一个执行器。
///
/// 用 `enum` 而不是 `Box<dyn Executor>`:`dispatch_task` 在热路径上
/// (`schedule_node` 的 `while try_claim` 每轮都调一次),静态分发省掉虚表;
/// 而且「关停 + join 必须发生在动节点之前」这条约定要看得清具体类型(见模块头)。
pub enum Executor {
    /// 拥有自己工作线程的线程池(YAML `type: "ThreadPoolExecutor"`)。
    Pool(ThreadPool),
    /// 不拥有线程,交还宿主线程(YAML `type: "DelegatingExecutor"`)。
    Delegating(DelegatingExecutor),
}

impl Executor {
    pub fn name(&self) -> &str {
        match self {
            Self::Pool(p) => p.name(),
            Self::Delegating(d) => d.name(),
        }
    }

    /// 是否把任务交还宿主线程(而不是自有工作线程)。
    pub fn is_delegating(&self) -> bool {
        matches!(self, Self::Delegating(_))
    }

    /// **自有**工作线程数。委托执行器返回 0 —— 它一个线程都不拥有。
    /// 「并行度是否 >1」的校验(`max_in_flight`)就是问这个。
    pub fn num_threads(&self) -> usize {
        match self {
            Self::Pool(p) => p.num_threads(),
            Self::Delegating(_) => 0,
        }
    }

    /// CPU 亲和力核列表(空 = 不绑)。仅用于内省 / 可视化。
    pub fn affinity(&self) -> &[usize] {
        match self {
            Self::Pool(p) => p.affinity(),
            Self::Delegating(_) => &[],
        }
    }

    /// 实时优先级(0 = 普通分时)。仅用于内省 / 可视化。
    pub fn priority(&self) -> i32 {
        match self {
            Self::Pool(p) => p.priority(),
            Self::Delegating(_) => 0,
        }
    }

    /// 拉起工作线程。委托执行器是 no-op:宿主线程早就在跑,不由引擎创建。
    pub fn start(&self, graph: Weak<GraphInner>) {
        match self {
            Self::Pool(p) => p.start(graph),
            Self::Delegating(_) => {}
        }
    }

    /// 投递一个就绪节点。返回 `false` 表示没收下,调用方须善后(撤销已取得的令牌)。
    pub fn submit(&self, node: NodeId) -> bool {
        match self {
            Self::Pool(p) => p.submit(node),
            Self::Delegating(d) => d.submit(node),
        }
    }

    pub fn submit_source_wake(&self, node: NodeId, generation: u64, delay: Duration) -> bool {
        match self {
            Self::Pool(p) => p.submit_source_wake(node, generation, delay),
            Self::Delegating(_) => false,
        }
    }

    pub fn pending(&self) -> usize {
        match self {
            Self::Pool(p) => p.pending(),
            Self::Delegating(d) => d.pending(),
        }
    }

    /// 关停并 join。幂等。委托执行器是 no-op(无线程可 join)。
    pub fn shutdown(&self) {
        match self {
            Self::Pool(p) => p.shutdown(),
            Self::Delegating(_) => {}
        }
    }

    /// 弹一个**待宿主执行**的任务;线程池恒返回 `None`(它的任务由自己的 worker 取)。
    pub fn take_delegated(&self) -> Option<NodeId> {
        match self {
            Self::Pool(_) => None,
            Self::Delegating(d) => d.take(),
        }
    }

    pub fn complete_delegated(&self) {
        if let Self::Delegating(d) = self {
            d.complete();
        }
    }

    pub(crate) fn stats(&self) -> ExecutorStatsSnapshot {
        match self {
            Self::Pool(p) => p.stats(),
            Self::Delegating(d) => d.stats(),
        }
    }

    /// 清空待宿主执行的队列(`reset` 用);线程池是 no-op。
    pub fn clear_delegated(&self) {
        match self {
            Self::Pool(_) => {}
            Self::Delegating(d) => d.clear(),
        }
    }

    pub fn reset_run_state(&self) {
        match self {
            Self::Pool(p) => {
                p.clear_delayed();
                p.reset_stats();
            }
            Self::Delegating(d) => {
                d.clear();
                d.reset_stats();
            }
        }
    }

    pub fn clear_delayed(&self) {
        if let Self::Pool(p) = self {
            p.clear_delayed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_threads_becomes_one() {
        let p = ThreadPool::new("t", 0, vec![], 0);
        assert_eq!(
            p.num_threads(),
            1,
            "a pool configured with zero threads is surely not intended"
        );
    }

    #[test]
    fn submit_before_start_is_queued() {
        let p = ThreadPool::new("t", 2, vec![], 0);
        assert!(p.submit(1));
        assert_eq!(p.pending(), 1, "tasks should be queued before start");
    }

    #[test]
    fn submit_after_shutdown_is_rejected() {
        let p = ThreadPool::new("t", 1, vec![], 0);
        p.shutdown();
        assert!(
            !p.submit(1),
            "after shutdown, submit must explicitly reject so the caller can release the token"
        );
    }

    #[test]
    fn shutdown_is_idempotent_and_joins() {
        let p = ThreadPool::new("t", 3, vec![], 0);
        // 不 start 也应能安全关停
        p.shutdown();
        p.shutdown();
    }

    #[test]
    fn delegating_queues_fifo_and_never_rejects() {
        let d = DelegatingExecutor::new(DEFAULT_EXECUTOR_NAME);
        assert!(d.submit(7));
        assert!(d.submit(8));
        assert_eq!(d.pending(), 2);
        // 宿主按入队顺序抽取 —— 这正是「执行顺序确定」的来源。
        assert_eq!(d.take(), Some(7));
        assert_eq!(d.take(), Some(8));
        assert_eq!(d.take(), None);
    }

    #[test]
    fn delegating_submit_survives_shutdown() {
        let e = Executor::Delegating(DelegatingExecutor::new("host"));
        e.shutdown(); // no-op:没有线程可关
        assert!(
            e.submit(1),
            "委托执行器没有『已关停』状态 —— 宿主线程不由引擎拉起,也无从 join"
        );
    }

    #[test]
    fn delegating_clear_drops_pending() {
        let d = DelegatingExecutor::new("host");
        d.submit(1);
        d.submit(2);
        d.clear();
        assert_eq!(d.pending(), 0, "reset 不能把上一轮的残留带进下一轮");
    }

    #[test]
    fn executor_kinds_report_their_shape() {
        let pool = Executor::Pool(ThreadPool::new("cpu", 4, vec![2, 3], 10));
        let host = Executor::Delegating(DelegatingExecutor::new(DEFAULT_EXECUTOR_NAME));

        assert!(!pool.is_delegating());
        assert_eq!(pool.name(), "cpu");
        assert_eq!(pool.num_threads(), 4);
        assert_eq!(pool.affinity(), &[2, 3]);
        assert_eq!(pool.priority(), 10);
        assert_eq!(pool.take_delegated(), None, "池的任务由它自己的 worker 取");

        assert!(host.is_delegating());
        assert_eq!(host.name(), DEFAULT_EXECUTOR_NAME);
        assert_eq!(
            host.num_threads(),
            0,
            "委托执行器一个线程都不拥有 —— max_in_flight 校验就是问这个"
        );
        assert!(host.affinity().is_empty());
        assert_eq!(host.priority(), 0);
        host.submit(5);
        assert_eq!(host.take_delegated(), Some(5));
    }

    /// Linux 下验证绑核**真的生效**:让工作线程回读自己的亲和力掩码,应恰为所绑的核。
    #[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
    #[test]
    fn affinity_actually_pins_worker_thread() {
        use std::sync::mpsc;

        extern "C" {
            fn sched_getaffinity(pid: i32, cpusetsize: usize, mask: *mut u64) -> i32;
        }
        // 至少要有 2 个核才谈得上「绑到某一个」;单核机器直接跳过。
        let ncpu = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if ncpu < 2 {
            return;
        }
        // 绑到 1 号核(存在,因为 ncpu>=2)。
        let (tx, rx) = mpsc::channel::<[u64; 16]>();
        let handle = std::thread::spawn(move || {
            super::pin_current_thread_to(1);
            let mut mask = [0u64; 16];
            let rc =
                unsafe { sched_getaffinity(0, std::mem::size_of_val(&mask), mask.as_mut_ptr()) };
            assert_eq!(rc, 0, "sched_getaffinity failed");
            tx.send(mask).unwrap();
        });
        let mask = rx.recv().unwrap();
        handle.join().unwrap();
        // 掩码应只置了 1 号核这一位。
        assert_eq!(
            mask[0],
            1u64 << 1,
            "worker must be pinned to exactly CPU 1, actual mask {:#b}",
            mask[0]
        );
    }

    /// 实时优先级是**尽力而为**的:有权限(CAP_SYS_NICE/root)时应真的切到 SCHED_FIFO;
    /// 无权限时静默失败、线程照常跑。两种情形都不能崩、不能改变功能。
    #[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
    #[test]
    fn rt_priority_is_best_effort() {
        extern "C" {
            fn sched_getscheduler(pid: i32) -> i32;
        }
        const SCHED_FIFO: i32 = 1;
        let handle = std::thread::spawn(|| {
            super::set_current_thread_rt_priority(10);
            unsafe { sched_getscheduler(0) }
        });
        let policy = handle.join().unwrap();
        // 有权限:应为 SCHED_FIFO;无权限:仍是原策略(通常 0=SCHED_OTHER)。都算通过。
        assert!(
            policy == SCHED_FIFO || policy >= 0,
            "setting priority must not leave the thread in an invalid scheduling state, actual policy={policy}"
        );
    }
}
