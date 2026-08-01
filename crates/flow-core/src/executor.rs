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
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// 把**当前线程**绑定到指定 CPU 核(Linux)。绑核是尽力而为的优化 ——
/// 其它平台、或核不存在等失败情形,一律静默降级为「不绑」,绝不影响正确性。
#[cfg(target_os = "linux")]
fn pin_current_thread_to(cpu: usize) {
    // 直接声明 glibc 早已链接进来的符号,避免引入 libc crate(本引擎坚持零外部 crate 依赖)。
    extern "C" {
        fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u64) -> i32;
    }
    const NBITS: usize = 1024; // glibc 默认 cpu_set_t 位宽
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

#[cfg(not(target_os = "linux"))]
fn pin_current_thread_to(_cpu: usize) {}

impl ThreadPool {
    pub fn new(name: &str, num_threads: usize, affinity: Vec<usize>) -> Self {
        Self {
            name: name.to_string(),
            // 0 视作 1:配了池却一个线程都没有肯定不是本意
            num_threads: num_threads.max(1),
            affinity,
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
            let h = std::thread::Builder::new()
                .name(tname)
                .spawn(move || {
                    if let Some(c) = cpu {
                        pin_current_thread_to(c);
                    }
                    worker(shared, weak)
                })
                .expect("无法创建工作线程");
            handles.push(h);
        }
    }

    /// 投递一个就绪节点。关停后返回 `false`,由调用方善后(释放已取得的令牌)。
    pub fn submit(&self, node: NodeId) -> bool {
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
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
        self.shared.stop.store(true, Ordering::SeqCst);
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
        let p = ThreadPool::new("t", 0, vec![]);
        assert_eq!(p.num_threads(), 1, "配了池却零线程肯定不是本意");
    }

    #[test]
    fn submit_before_start_is_queued() {
        let p = ThreadPool::new("t", 2, vec![]);
        assert!(p.submit(1));
        assert_eq!(p.pending(), 1, "未启动时任务应先排队");
    }

    #[test]
    fn submit_after_shutdown_is_rejected() {
        let p = ThreadPool::new("t", 1, vec![]);
        p.shutdown();
        assert!(!p.submit(1), "关停后必须明确拒绝,让调用方能释放令牌");
    }

    #[test]
    fn shutdown_is_idempotent_and_joins() {
        let p = ThreadPool::new("t", 3, vec![]);
        // 不 start 也应能安全关停
        p.shutdown();
        p.shutdown();
    }

    /// Linux 下验证绑核**真的生效**:让工作线程回读自己的亲和力掩码,应恰为所绑的核。
    #[cfg(target_os = "linux")]
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
            assert_eq!(rc, 0, "sched_getaffinity 失败");
            tx.send(mask).unwrap();
        });
        let mask = rx.recv().unwrap();
        handle.join().unwrap();
        // 掩码应只置了 1 号核这一位。
        assert_eq!(
            mask[0],
            1u64 << 1,
            "worker 必须被恰好绑到 1 号核,实际掩码 {:#b}",
            mask[0]
        );
    }
}
