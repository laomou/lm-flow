//! 拉模式输出 Poller：有界队列、溢出策略、宿主等待与背压统计。
//!
//! Poller 会在图的派发线程上接收输出，因此其 `Block` 策略与图调度存在直接交互；
//! 相关实现集中在这里，`mod.rs` 只保留边派发和生命周期核心。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::packet::Packet;
use crate::runtime;
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

use super::{BackpressureStats, EdgeId, Graph, GraphInner, State};

pub(super) struct PollerInner {
    pub(super) edge: EdgeId,
    pub(super) edge_name: String,
    pub(super) queue: Mutex<VecDeque<Packet>>,
    pub(super) closed: AtomicBool,
    pub(super) capacity: Option<usize>,
    pub(super) overflow: PollerOverflow,
    /// `Block` 等宿主腾位的上界；`None` = 无上界。
    pub(super) block_timeout: Option<std::time::Duration>,
    pub(super) dropped: AtomicU64,
    pub(super) block_backpressure: BackpressureStats,
    pub(super) active: AtomicBool,
    pub(super) observe_timestamp_bounds: bool,
}

impl PollerInner {
    pub(super) fn push(&self, graph: &GraphInner, packet: Packet) -> bool {
        let mut deadline: Option<std::time::Instant> = None;
        let mut blocked = false;
        loop {
            if !self.active.load(Ordering::SeqCst) {
                if blocked {
                    self.finish_block(graph, false);
                }
                return false;
            }
            let before = graph.activity_gen();
            let mut queue = self.queue.lock().expect("poller lock poisoned");
            if !self.active.load(Ordering::SeqCst) {
                drop(queue);
                if blocked {
                    self.finish_block(graph, false);
                }
                return false;
            }
            let full = self
                .capacity
                .is_some_and(|capacity| queue.len() >= capacity);
            if !full {
                graph.shared.on_enqueue(packet.byte_size());
                queue.push_back(packet);
                drop(queue);
                if blocked {
                    self.finish_block(graph, true);
                }
                return true;
            }
            match self.overflow {
                PollerOverflow::Block => {
                    let queued = queue.len();
                    drop(queue);
                    if !blocked {
                        blocked = true;
                        self.begin_block(graph, queued);
                    }
                    if !self.active.load(Ordering::SeqCst)
                        || graph.shared.is_cancelled()
                        || graph.shared.has_error()
                    {
                        self.finish_block(graph, false);
                        return false;
                    }
                    if let Some(limit) = self.block_timeout {
                        if deadline.is_none() {
                            deadline = Some(std::time::Instant::now() + limit);
                        }
                        if let Some(at) = deadline {
                            if std::time::Instant::now() >= at {
                                self.finish_block(graph, false);
                                graph.shared.record_error(Error::Kernel(format!(
                                    "poller on output port `{}`: blocked for {:?} waiting for the \
                                     host to free a slot (capacity {}, queued {}). `PollerOverflow::Block` \
                                     requires a host that drains the poller *concurrently*; a host \
                                     that sends/waits first and drains afterwards will deadlock, \
                                     because the producer parks inside dispatch and never reaches \
                                     `poller.next()`. Use `Latest`/`DropOldest`/`DropNewest`, raise \
                                     the capacity, drain from another thread, or set \
                                     `PollerOptions::with_block_timeout(None)` if you are certain a \
                                     concurrent drainer exists.",
                                    self.edge_name,
                                    limit,
                                    self.capacity.unwrap_or(0),
                                    queued,
                                )));
                                return false;
                            }
                        }
                    }
                    graph.wait_activity_since(before, std::time::Duration::from_millis(100));
                }
                PollerOverflow::DropOldest => {
                    let dropped = if let Some(old) = queue.pop_front() {
                        graph.shared.on_dequeue(old.byte_size());
                        1
                    } else {
                        0
                    };
                    graph.shared.on_enqueue(packet.byte_size());
                    queue.push_back(packet);
                    let queued = queue.len();
                    drop(queue);
                    if dropped != 0 {
                        let total = self.record_dropped(dropped);
                        self.log_dropped(total, queued);
                    }
                    return true;
                }
                PollerOverflow::DropNewest => {
                    let queued = queue.len();
                    drop(queue);
                    let total = self.record_dropped(1);
                    self.log_dropped(total, queued);
                    return false;
                }
                PollerOverflow::Latest => {
                    let dropped = queue.len() as u64;
                    while let Some(old) = queue.pop_front() {
                        graph.shared.on_dequeue(old.byte_size());
                    }
                    graph.shared.on_enqueue(packet.byte_size());
                    queue.push_back(packet);
                    let queued = queue.len();
                    drop(queue);
                    if dropped != 0 {
                        let total = self.record_dropped(dropped);
                        self.log_dropped(total, queued);
                    }
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

    pub(super) fn clear(&self, graph: &GraphInner) {
        let mut queue = self.queue.lock().expect("poller lock poisoned");
        while let Some(packet) = queue.pop_front() {
            graph.shared.on_dequeue(packet.byte_size());
        }
        drop(queue);
        graph.notify_activity();
    }

    fn begin_block(&self, graph: &GraphInner, queued: usize) {
        let Some(event) = self.block_backpressure.enter(graph.epoch_us()) else {
            return;
        };
        if event.is_power_of_two() {
            runtime::log_warn(&format!(
                "poller backpressure #{event}: output `{}` blocked (policy=Block, capacity={}, \
                 queued={queued}, timeout={}); waiting for the host to drain concurrently",
                self.edge_name,
                self.capacity.unwrap_or(0),
                self.block_timeout.map_or_else(
                    || "none".to_string(),
                    |timeout| format!("{}ms", timeout.as_millis())
                ),
            ));
        }
    }

    fn finish_block(&self, graph: &GraphInner, log_recovery: bool) {
        let Some((event, elapsed)) = self.block_backpressure.leave(graph.epoch_us()) else {
            return;
        };
        if log_recovery && event.is_power_of_two() {
            runtime::log_info(&format!(
                "poller backpressure #{event} cleared: output `{}` resumed after {elapsed}us",
                self.edge_name,
            ));
        }
    }

    fn record_dropped(&self, count: u64) -> u64 {
        let before = self.dropped.fetch_add(count, Ordering::Relaxed);
        before + count
    }

    fn log_dropped(&self, total: u64, queued: usize) {
        if total == 1 || total.is_power_of_two() {
            runtime::log_warn(&format!(
                "poller overflow: output `{}` policy={:?} capacity={} queued={queued} \
                 dropped_total={total}",
                self.edge_name,
                self.overflow,
                self.capacity.unwrap_or(0),
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
    /// `Block` 溢出策略下，等宿主腾出一个槽位的上界。`None` 表示无上界，
    /// 仅适用于宿主确定会在另一个线程持续排水的场景。
    pub block_timeout: Option<std::time::Duration>,
}

impl PollerOptions {
    /// `Block` 默认带 5 秒上界；有损策略下该字段无意义。
    pub fn new(capacity: usize, overflow: PollerOverflow) -> Self {
        Self {
            capacity,
            overflow,
            block_timeout: Some(std::time::Duration::from_secs(5)),
        }
    }

    /// 覆盖 `Block` 的等待上界。
    pub fn with_block_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.block_timeout = timeout;
        self
    }
}

/// 拉模式输出句柄。
pub struct Poller {
    graph: Arc<GraphInner>,
    inner: Arc<PollerInner>,
}

/// A typed event from an output poller that observes timestamp bounds.
#[derive(Clone, Debug)]
pub enum OutputEvent {
    Packet(Packet),
    TimestampBound(Timestamp),
    Done,
}

impl Graph {
    pub fn add_poller(&self, port: &str) -> Result<Poller> {
        self.add_poller_inner(port, None, false)
    }

    /// Add an unbounded poller that also receives timestamp-bound events as empty packets.
    pub fn add_poller_with_timestamp_bounds(&self, port: &str) -> Result<Poller> {
        self.add_poller_inner(port, None, true)
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
        self.add_poller_inner(port, Some(options), false)
    }

    fn add_poller_inner(
        &self,
        port: &str,
        options: Option<PollerOptions>,
        observe_timestamp_bounds: bool,
    ) -> Result<Poller> {
        let state = self.state();
        if state != State::Initialized {
            return Err(Error::State(format!(
                "add_poller must be called before start (current state {state:?})"
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
            block_timeout: options.and_then(|options| options.block_timeout),
            dropped: AtomicU64::new(0),
            block_backpressure: BackpressureStats::default(),
            active: AtomicBool::new(true),
            observe_timestamp_bounds,
        });
        if observe_timestamp_bounds {
            self.inner.edges[edge]
                .has_timestamp_bound_subscriber
                .store(true, Ordering::Relaxed);
        }
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
}

impl Poller {
    fn classify_event(packet: Packet) -> OutputEvent {
        if !packet.is_empty() {
            return OutputEvent::Packet(packet);
        }
        if packet.timestamp() == Timestamp::done() {
            OutputEvent::Done
        } else {
            OutputEvent::TimestampBound(packet.timestamp())
        }
    }

    /// Get the next output as a typed packet, timestamp-bound, or done event.
    ///
    /// Use a poller created by [`Graph::add_poller_with_timestamp_bounds`] to receive all three
    /// variants. A normal poller only yields [`OutputEvent::Packet`] and ends with `None`.
    pub fn next_event(&self) -> Option<OutputEvent> {
        self.next().map(Self::classify_event)
    }

    /// Timed form of [`next_event`](Self::next_event).
    pub fn next_event_timeout(&self, timeout: std::time::Duration) -> Result<Option<OutputEvent>> {
        self.next_timeout(timeout)
            .map(|packet| packet.map(Self::classify_event))
    }

    /// Non-blocking form of [`next_event`](Self::next_event).
    pub fn try_next_event(&self) -> Option<OutputEvent> {
        self.try_next().map(Self::classify_event)
    }

    /// 取下一个包。等待期间会抽取并执行主线程任务。
    pub fn next(&self) -> Option<Packet> {
        self.next_deadline(None).ok().flatten()
    }

    /// 带超时：`Ok(Some)` 取到，`Ok(None)` 图已结束，`Err(Timeout)` 超时。
    pub fn next_timeout(&self, timeout: std::time::Duration) -> Result<Option<Packet>> {
        self.next_deadline(Some(std::time::Instant::now() + timeout))
    }

    fn next_deadline(&self, deadline: Option<std::time::Instant>) -> Result<Option<Packet>> {
        loop {
            if let Some(packet) = self.inner.pop(&self.graph) {
                return Ok(Some(packet));
            }
            if self.inner.closed.load(Ordering::SeqCst) || self.graph.shared.has_error() {
                return Ok(self.inner.pop(&self.graph));
            }
            if self.graph.pump_step() {
                continue;
            }
            let before = self.graph.activity_gen_pub();
            if self.graph.is_idle_pub() {
                return Ok(self.inner.pop(&self.graph));
            }
            match self.graph.remaining_for_poller(deadline) {
                Some(duration) => self.graph.wait_activity_since_pub(before, duration),
                None => return Err(Error::Timeout),
            }
        }
    }

    /// 非阻塞：仅看现有队列。
    pub fn try_next(&self) -> Option<Packet> {
        self.inner.pop(&self.graph)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    pub fn backpressure_stats(&self) -> PollerBackpressureStatsSnapshot {
        let stats = self
            .inner
            .block_backpressure
            .snapshot(self.graph.epoch_us());
        PollerBackpressureStatsSnapshot {
            port_name: self.inner.edge_name.clone(),
            capacity: self.inner.capacity,
            overflow: self.inner.overflow,
            queued_packets: self.inner.queue.lock().expect("poller lock poisoned").len(),
            dropped_packets: self.inner.dropped.load(Ordering::Relaxed),
            blocked: stats.blocked,
            active_waiters: stats.active_waiters,
            blocked_for_us: stats.blocked_for_us,
            block_events: stats.block_events,
            total_blocked_us: stats.total_blocked_us,
        }
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

impl std::fmt::Debug for Poller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Poller{{port:`{}`, pending:{}}}",
            self.graph.edges[self.inner.edge].name,
            self.inner
                .queue
                .lock()
                .map(|queue| queue.len())
                .unwrap_or(0)
        )
    }
}

#[derive(Debug, Clone)]
pub struct PollerBackpressureStatsSnapshot {
    pub port_name: String,
    pub capacity: Option<usize>,
    pub overflow: PollerOverflow,
    pub queued_packets: usize,
    pub dropped_packets: u64,
    pub blocked: bool,
    pub active_waiters: usize,
    pub blocked_for_us: u64,
    pub block_events: u64,
    pub total_blocked_us: u64,
}
