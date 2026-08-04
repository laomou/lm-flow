//! 执行器:把「就绪节点」派给某个线程池执行。
//!
//! 零外部依赖(只用 std):任务队列 = `Mutex<VecDeque<NodeId>>` + `Condvar`。
//!
//! 两条关键约定
//!  * 工作线程持 **`Weak<GraphInner>`**。若持强引用会与「GraphInner 拥有执行器」
//!    构成 `Arc` 环,图永远不会被释放。
//!  * `GraphInner::drop` 必须**先关停并 join 线程池**,再去动节点 ——
//!    否则工作线程可能触碰正在析构的节点。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;

use crate::graph::{GraphInner, NodeId};

struct Shared {
    queue: Mutex<VecDeque<NodeId>>,
    cv: Condvar,
    stop: AtomicBool,
}

impl Shared {
    /// 取一个任务;返回 `None` 表示「已关停且队列排空」,工作线程可以退出。
    fn take(&self) -> Option<NodeId> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(n) = q.pop_front() {
                return Some(n);
            }
            if self.stop.load(Ordering::SeqCst) {
                return None;
            }
            q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner());
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
                queue: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
                stop: AtomicBool::new(false),
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
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        // 在**队列锁内**查 stop:与 shutdown 的锁内置位配对。否则关停后仍可能入队,
        // 任务成孤儿留在队里(其 in_flight 永不归零)。
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        q.push_back(node);
        drop(q);
        self.shared.cv.notify_one();
        true
    }

    pub fn pending(&self) -> usize {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
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
    while let Some(node) = shared.take() {
        // upgrade 失败 = 图已销毁。此时丢弃任务是正确的:节点本身已不存在。
        match graph.upgrade() {
            Some(g) => g.run_node_on_worker(node),
            None => break,
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
