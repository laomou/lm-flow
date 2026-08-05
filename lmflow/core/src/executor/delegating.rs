use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::graph::NodeId;

use super::stats::{ExecutorStats, ExecutorStatsSnapshot};

struct QueuedTask {
    enqueued_at: Instant,
    node: NodeId,
}

/// 委托执行器:一个线程都不拥有,把就绪节点**交还宿主线程**跑。
///
/// `submit` 只入队，真正执行发生在宿主进入阻塞接口或主动调用 `Graph::pump_step` 时。
/// 同一张图的委托任务由图级原子闸门保证零并发。
pub struct DelegatingExecutor {
    name: String,
    queue: Mutex<VecDeque<QueuedTask>>,
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

    /// 入队待宿主抽取。委托执行器没有已关停状态，故恒返回 `true`。
    pub fn submit(&self, node: NodeId) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        queue.push_back(QueuedTask {
            enqueued_at: Instant::now(),
            node,
        });
        self.stats.enqueued();
        true
    }

    pub fn pending(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    /// 弹一个待办交给宿主线程跑;`None` = 队列空。
    pub fn take(&self) -> Option<NodeId> {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        let task = queue.pop_front()?;
        self.stats.started(task.enqueued_at.elapsed());
        Some(task.node)
    }

    pub fn complete(&self, execution: Duration) {
        self.stats.completed(execution);
    }

    pub(crate) fn stats(&self) -> ExecutorStatsSnapshot {
        self.stats.snapshot()
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        !self
            .queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
            || self.stats.has_running()
    }

    pub(crate) fn queued_nodes(&self) -> Vec<NodeId> {
        self.queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|task| task.node)
            .collect()
    }

    /// 清空队列(`reset` 用:上一轮的残留不能带进下一轮)。
    pub fn clear(&self) {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        let count = queue.len();
        queue.clear();
        self.stats.dropped(count);
    }

    pub fn reset_stats(&self) {
        self.stats.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queues_fifo_and_never_rejects() {
        let executor = DelegatingExecutor::new("host");
        assert!(executor.submit(7));
        assert!(executor.submit(8));
        assert_eq!(executor.pending(), 2);
        assert_eq!(executor.take(), Some(7));
        assert_eq!(executor.take(), Some(8));
        assert_eq!(executor.take(), None);
    }

    #[test]
    fn clear_drops_pending() {
        let executor = DelegatingExecutor::new("host");
        executor.submit(1);
        executor.submit(2);
        executor.clear();
        assert_eq!(executor.pending(), 0);
    }
}
