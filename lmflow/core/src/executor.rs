//! 执行器:把「就绪节点」派给某个执行器执行。
//!
//! 两种执行器,由 YAML 的 `executors[].type` 选:
//!  * [`ThreadPool`] —— 拥有自己的工作线程,真并发。**默认执行器就是它**。
//!  * [`DelegatingExecutor`] —— 一个线程都不拥有,把任务交还**宿主线程**跑。
//!
//! 两者对调度器是同一个东西([`Executor`]),`GraphInner::dispatch_task` 不为谁分叉。
//! 具体实现分别位于 `executor/thread_pool.rs` 与 `executor/delegating.rs`。

use std::sync::Weak;
use std::time::Duration;

use crate::graph::{GraphInner, NodeId};

mod delegating;
mod platform;
mod stats;
mod thread_pool;

pub use delegating::DelegatingExecutor;
pub(crate) use stats::ExecutorStatsSnapshot;
pub use thread_pool::ThreadPool;

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
/// 而且「关停 + join 必须发生在动节点之前」这条约定要看得清具体类型。
pub enum Executor {
    /// 拥有自己工作线程的线程池(YAML `type: "ThreadPoolExecutor"`)。
    Pool(ThreadPool),
    /// 不拥有线程,交还宿主线程(YAML `type: "DelegatingExecutor"`)。
    Delegating(DelegatingExecutor),
}

impl Executor {
    pub fn name(&self) -> &str {
        match self {
            Self::Pool(pool) => pool.name(),
            Self::Delegating(delegating) => delegating.name(),
        }
    }

    /// 是否把任务交还宿主线程(而不是自有工作线程)。
    pub fn is_delegating(&self) -> bool {
        matches!(self, Self::Delegating(_))
    }

    /// **自有**工作线程数。委托执行器返回 0 —— 它一个线程都不拥有。
    pub fn num_threads(&self) -> usize {
        match self {
            Self::Pool(pool) => pool.num_threads(),
            Self::Delegating(_) => 0,
        }
    }

    /// CPU 亲和力核列表(空 = 不绑)。仅用于内省 / 可视化。
    pub fn affinity(&self) -> &[usize] {
        match self {
            Self::Pool(pool) => pool.affinity(),
            Self::Delegating(_) => &[],
        }
    }

    /// 实时优先级(0 = 普通分时)。仅用于内省 / 可视化。
    pub fn priority(&self) -> i32 {
        match self {
            Self::Pool(pool) => pool.priority(),
            Self::Delegating(_) => 0,
        }
    }

    /// 拉起工作线程。委托执行器是 no-op:宿主线程早就在跑,不由引擎创建。
    pub fn start(&self, graph: Weak<GraphInner>) {
        if let Self::Pool(pool) = self {
            pool.start(graph);
        }
    }

    /// 投递一个就绪节点。返回 `false` 表示没收下,调用方须善后(撤销已取得的令牌)。
    pub fn submit(&self, node: NodeId) -> bool {
        match self {
            Self::Pool(pool) => pool.submit(node),
            Self::Delegating(delegating) => delegating.submit(node),
        }
    }

    pub fn submit_source_wake(&self, node: NodeId, generation: u64, delay: Duration) -> bool {
        match self {
            Self::Pool(pool) => pool.submit_source_wake(node, generation, delay),
            Self::Delegating(_) => false,
        }
    }

    pub fn pending(&self) -> usize {
        match self {
            Self::Pool(pool) => pool.pending(),
            Self::Delegating(delegating) => delegating.pending(),
        }
    }

    /// 关停并 join。幂等。委托执行器是 no-op(无线程可 join)。
    pub fn shutdown(&self) {
        if let Self::Pool(pool) = self {
            pool.shutdown();
        }
    }

    /// 弹一个**待宿主执行**的任务;线程池恒返回 `None`(它的任务由自己的 worker 取)。
    pub fn take_delegated(&self) -> Option<NodeId> {
        match self {
            Self::Pool(_) => None,
            Self::Delegating(delegating) => delegating.take(),
        }
    }

    pub fn complete_delegated(&self, execution: Duration) {
        if let Self::Delegating(delegating) = self {
            delegating.complete(execution);
        }
    }

    pub(crate) fn stats(&self) -> ExecutorStatsSnapshot {
        match self {
            Self::Pool(pool) => pool.stats(),
            Self::Delegating(delegating) => delegating.stats(),
        }
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        match self {
            Self::Pool(pool) => pool.has_pending_work(),
            Self::Delegating(delegating) => delegating.has_pending_work(),
        }
    }

    pub(crate) fn queued_nodes(&self) -> Vec<NodeId> {
        match self {
            Self::Pool(pool) => pool.queued_nodes(),
            Self::Delegating(delegating) => delegating.queued_nodes(),
        }
    }

    /// 清空待宿主执行的队列(`reset` 用);线程池是 no-op。
    pub fn clear_delegated(&self) {
        if let Self::Delegating(delegating) = self {
            delegating.clear();
        }
    }

    pub fn reset_run_state(&self) {
        match self {
            Self::Pool(pool) => {
                pool.clear_delayed();
                pool.reset_stats();
            }
            Self::Delegating(delegating) => {
                delegating.clear();
                delegating.reset_stats();
            }
        }
    }

    pub fn clear_delayed(&self) {
        if let Self::Pool(pool) = self {
            pool.clear_delayed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegating_submit_survives_shutdown() {
        let executor = Executor::Delegating(DelegatingExecutor::new("host"));
        executor.shutdown();
        assert!(
            executor.submit(1),
            "委托执行器没有『已关停』状态 —— 宿主线程不由引擎拉起,也无从 join"
        );
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
        assert_eq!(pool.take_delegated(), None);

        assert!(host.is_delegating());
        assert_eq!(host.name(), DEFAULT_EXECUTOR_NAME);
        assert_eq!(host.num_threads(), 0);
        assert!(host.affinity().is_empty());
        assert_eq!(host.priority(), 0);
        host.submit(5);
        assert_eq!(host.take_delegated(), Some(5));
    }
}
