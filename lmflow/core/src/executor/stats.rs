use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::config::StatsLevel;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExecutorStatsSnapshot {
    pub queued: usize,
    pub running: usize,
    pub peak_queued: usize,
    pub completed: u64,
    pub total_wait_us: u64,
    pub total_execution_us: u64,
    pub queued_for_us: u64,
}

pub(super) struct ExecutorStats {
    full: bool,
    epoch: Instant,
    queued: AtomicUsize,
    running: AtomicUsize,
    peak_queued: AtomicUsize,
    completed: AtomicU64,
    total_wait_us: AtomicU64,
    total_execution_us: AtomicU64,
    queued_since_us: AtomicU64,
}

impl Default for ExecutorStats {
    fn default() -> Self {
        Self::new(StatsLevel::Full)
    }
}

impl ExecutorStats {
    pub(super) fn new(level: StatsLevel) -> Self {
        Self {
            full: level == StatsLevel::Full,
            epoch: Instant::now(),
            queued: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            peak_queued: AtomicUsize::new(0),
            completed: AtomicU64::new(0),
            total_wait_us: AtomicU64::new(0),
            total_execution_us: AtomicU64::new(0),
            queued_since_us: AtomicU64::new(0),
        }
    }

    pub(super) fn full(&self) -> bool {
        self.full
    }

    pub(super) fn enqueued(&self) {
        let before = self.queued.fetch_add(1, Ordering::Relaxed);
        if !self.full {
            return;
        }
        let queued = before + 1;
        if before == 0 {
            self.queued_since_us.store(
                duration_micros(self.epoch.elapsed()).saturating_add(1),
                Ordering::Relaxed,
            );
        }
        self.peak_queued.fetch_max(queued, Ordering::Relaxed);
    }

    pub(super) fn started(&self, wait: Option<Duration>) {
        let before = self.queued.fetch_sub(1, Ordering::Relaxed);
        if self.full && before == 1 {
            self.queued_since_us.store(0, Ordering::Relaxed);
        }
        self.running.fetch_add(1, Ordering::Relaxed);
        if let Some(wait) = wait {
            self.total_wait_us
                .fetch_add(duration_micros(wait), Ordering::Relaxed);
        }
    }

    pub(super) fn completed(&self, execution: Option<Duration>) {
        self.running.fetch_sub(1, Ordering::Relaxed);
        if !self.full {
            return;
        }
        self.completed.fetch_add(1, Ordering::Relaxed);
        if let Some(execution) = execution {
            self.total_execution_us
                .fetch_add(duration_micros(execution), Ordering::Relaxed);
        }
    }

    pub(super) fn dropped(&self, count: usize) {
        let before = self
            .queued
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
                Some(queued.saturating_sub(count))
            })
            .unwrap_or_else(|queued| queued);
        if self.full && before <= count {
            self.queued_since_us.store(0, Ordering::Relaxed);
        }
    }

    pub(super) fn snapshot(&self) -> ExecutorStatsSnapshot {
        let queued_since = self.queued_since_us.load(Ordering::Relaxed);
        ExecutorStatsSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            running: self.running.load(Ordering::Relaxed),
            peak_queued: self.peak_queued.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            total_wait_us: self.total_wait_us.load(Ordering::Relaxed),
            total_execution_us: self.total_execution_us.load(Ordering::Relaxed),
            queued_for_us: if !self.full || queued_since == 0 {
                0
            } else {
                duration_micros(self.epoch.elapsed()).saturating_sub(queued_since.saturating_sub(1))
            },
        }
    }

    pub(super) fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub(super) fn has_running(&self) -> bool {
        self.running.load(Ordering::Relaxed) != 0
    }

    pub(super) fn reset(&self) {
        self.queued.store(0, Ordering::Relaxed);
        self.running.store(0, Ordering::Relaxed);
        self.peak_queued.store(0, Ordering::Relaxed);
        self.completed.store(0, Ordering::Relaxed);
        self.total_wait_us.store(0, Ordering::Relaxed);
        self.total_execution_us.store(0, Ordering::Relaxed);
        self.queued_since_us.store(0, Ordering::Relaxed);
    }
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}
