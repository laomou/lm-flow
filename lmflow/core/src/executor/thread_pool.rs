use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::graph::{GraphInner, NodeId};

use super::platform::{pin_current_thread_to, set_current_thread_rt_priority};
use super::stats::{ExecutorStats, ExecutorStatsSnapshot};

#[derive(Clone, Copy)]
enum Task {
    Run(NodeId),
    WakeSource(NodeId, u64),
}

struct QueuedTask {
    enqueued_at: Instant,
    task: Task,
}

struct DelayedTask {
    deadline: Instant,
    task: Task,
}

#[derive(Default)]
struct QueueState {
    ready: VecDeque<QueuedTask>,
    delayed: Vec<DelayedTask>,
}

struct Shared {
    queue: Mutex<QueueState>,
    cv: Condvar,
    stop: AtomicBool,
    stats: ExecutorStats,
}

impl Shared {
    /// 取一个任务;返回 `None` 表示「已关停且队列排空」。
    fn take(&self) -> Option<QueuedTask> {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            let now = Instant::now();
            while queue
                .delayed
                .first()
                .is_some_and(|delayed| delayed.deadline <= now)
            {
                let delayed = queue.delayed.remove(0);
                queue.ready.push_back(QueuedTask {
                    enqueued_at: now,
                    task: delayed.task,
                });
                self.stats.enqueued();
            }
            if let Some(task) = queue.ready.pop_front() {
                self.stats.started(task.enqueued_at.elapsed());
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
                    .unwrap_or_else(|error| error.into_inner());
                queue = next;
            } else {
                queue = self
                    .cv
                    .wait(queue)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
    }
}

pub struct ThreadPool {
    name: String,
    num_threads: usize,
    affinity: Vec<usize>,
    priority: i32,
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl ThreadPool {
    pub fn new(name: &str, num_threads: usize, affinity: Vec<usize>, priority: i32) -> Self {
        Self {
            name: name.to_string(),
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

    pub fn affinity(&self) -> &[usize] {
        &self.affinity
    }

    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// 拉起工作线程。必须在 `Arc<GraphInner>` 已存在之后调用。
    pub fn start(&self, graph: Weak<GraphInner>) {
        let mut handles = self
            .threads
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !handles.is_empty() {
            return;
        }
        for index in 0..self.num_threads {
            let shared = self.shared.clone();
            let graph = graph.clone();
            let thread_name = format!("{}-{index}", self.name);
            let cpu = if self.affinity.is_empty() {
                None
            } else {
                Some(self.affinity[index % self.affinity.len()])
            };
            let priority = self.priority;
            let handle = std::thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    if let Some(cpu) = cpu {
                        pin_current_thread_to(cpu);
                    }
                    if priority > 0 {
                        set_current_thread_rt_priority(priority);
                    }
                    worker(shared, graph);
                })
                .expect("failed to create worker thread");
            handles.push(handle);
        }
    }

    /// 投递一个就绪节点。关停后返回 `false`。
    pub fn submit(&self, node: NodeId) -> bool {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        queue.ready.push_back(QueuedTask {
            enqueued_at: Instant::now(),
            task: Task::Run(node),
        });
        self.shared.stats.enqueued();
        drop(queue);
        self.shared.cv.notify_one();
        true
    }

    pub fn submit_source_wake(&self, node: NodeId, generation: u64, delay: Duration) -> bool {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        let task = Task::WakeSource(node, generation);
        if delay.is_zero() {
            queue.ready.push_back(QueuedTask {
                enqueued_at: Instant::now(),
                task,
            });
            self.shared.stats.enqueued();
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
        drop(queue);
        self.shared.cv.notify_one();
        true
    }

    pub fn pending(&self) -> usize {
        self.shared.stats.queued()
    }

    pub fn clear_delayed(&self) {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        queue.delayed.clear();
        drop(queue);
        self.shared.cv.notify_all();
    }

    pub(crate) fn stats(&self) -> ExecutorStatsSnapshot {
        self.shared.stats.snapshot()
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        let queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        !queue.ready.is_empty() || !queue.delayed.is_empty() || self.shared.stats.has_running()
    }

    pub(crate) fn queued_nodes(&self) -> Vec<NodeId> {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .ready
            .iter()
            .filter_map(|task| match task.task {
                Task::Run(node) => Some(node),
                Task::WakeSource(_, _) => None,
            })
            .collect()
    }

    pub fn reset_stats(&self) {
        self.shared.stats.reset();
    }

    /// 关停并 join。幂等。
    pub fn shutdown(&self) {
        {
            let _queue = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.shared.stop.store(true, Ordering::SeqCst);
        }
        self.shared.cv.notify_all();
        let handles = {
            let mut handles = self
                .threads
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *handles)
        };
        for handle in handles {
            let _ = handle.join();
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
        let Some(graph) = graph.upgrade() else {
            break;
        };
        let started = Instant::now();
        match task.task {
            Task::Run(node) => graph.run_node_on_worker(node),
            Task::WakeSource(node, generation) => graph.wake_source_on_worker(node, generation),
        }
        shared.stats.completed(started.elapsed());
        graph.executor_task_completed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_threads_becomes_one() {
        let pool = ThreadPool::new("test", 0, vec![], 0);
        assert_eq!(pool.num_threads(), 1);
    }

    #[test]
    fn submit_before_start_is_queued() {
        let pool = ThreadPool::new("test", 2, vec![], 0);
        assert!(pool.submit(1));
        assert_eq!(pool.pending(), 1);
    }

    #[test]
    fn submit_after_shutdown_is_rejected() {
        let pool = ThreadPool::new("test", 1, vec![], 0);
        pool.shutdown();
        assert!(!pool.submit(1));
    }

    #[test]
    fn shutdown_is_idempotent_and_joins() {
        let pool = ThreadPool::new("test", 3, vec![], 0);
        pool.shutdown();
        pool.shutdown();
    }
}
