use super::*;

// Internal queue and watermark backpressure.

#[derive(Debug, Default)]
pub(super) struct BackpressureStats {
    pub(super) active_waiters: AtomicUsize,
    pub(super) block_events: AtomicU64,
    pub(super) blocked_total_us: AtomicU64,
    pub(super) blocked_since_us: AtomicI64,
}

impl BackpressureStats {
    pub(super) fn enter(&self, now_us: i64) -> Option<u64> {
        let before = self.active_waiters.fetch_add(1, Ordering::SeqCst);
        if before != 0 {
            return None;
        }
        self.blocked_since_us
            .store(now_us.saturating_add(1), Ordering::SeqCst);
        Some(self.block_events.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub(super) fn leave(&self, now_us: i64) -> Option<(u64, u64)> {
        let before = self.active_waiters.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(before > 0);
        if before != 1 {
            return None;
        }
        let since = self.blocked_since_us.swap(0, Ordering::SeqCst);
        let elapsed = now_us.saturating_sub(since.saturating_sub(1)).max(0) as u64;
        self.blocked_total_us.fetch_add(elapsed, Ordering::Relaxed);
        Some((self.block_events.load(Ordering::Relaxed), elapsed))
    }

    pub(super) fn snapshot(&self, now_us: i64) -> BackpressureStatsSnapshot {
        let since = self.blocked_since_us.load(Ordering::SeqCst);
        let blocked_for_us = if since == 0 {
            0
        } else {
            now_us.saturating_sub(since.saturating_sub(1)).max(0) as u64
        };
        BackpressureStatsSnapshot {
            blocked: self.active_waiters.load(Ordering::SeqCst) != 0,
            active_waiters: self.active_waiters.load(Ordering::SeqCst),
            blocked_for_us,
            block_events: self.block_events.load(Ordering::Relaxed),
            total_blocked_us: self
                .blocked_total_us
                .load(Ordering::Relaxed)
                .saturating_add(blocked_for_us),
        }
    }

    pub(super) fn reset(&self) {
        self.active_waiters.store(0, Ordering::SeqCst);
        self.block_events.store(0, Ordering::Relaxed);
        self.blocked_total_us.store(0, Ordering::Relaxed);
        self.blocked_since_us.store(0, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum BlockedFlush {
    Invocation { slot: usize, ok: bool },
    Close,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct InputQueueReservation {
    pub(super) node: NodeId,
    pub(super) port: usize,
    pub(super) packets: usize,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct InputQueueBlockContext {
    pub(super) producer: NodeId,
    pub(super) consumer: NodeId,
    pub(super) port: usize,
    pub(super) capacity: usize,
    pub(super) queued: usize,
    pub(super) reserved: usize,
    pub(super) incoming: usize,
}

#[derive(Debug, Default)]
pub(super) struct InputQueueStats {
    pub(super) peak_packets: AtomicUsize,
    pub(super) peak_bytes: AtomicU64,
    pub(super) block_events: AtomicU64,
    pub(super) blocked_total_us: AtomicU64,
    /// 0 = 当前未阻塞；否则为相对 graph epoch 的微秒数 + 1。
    pub(super) blocked_since_us: AtomicI64,
}

impl InputQueueStats {
    pub(super) fn reset(&self) {
        self.peak_packets.store(0, Ordering::Relaxed);
        self.peak_bytes.store(0, Ordering::Relaxed);
        self.block_events.store(0, Ordering::Relaxed);
        self.blocked_total_us.store(0, Ordering::Relaxed);
        self.blocked_since_us.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct BackpressureStatsSnapshot {
    pub(super) blocked: bool,
    pub(super) active_waiters: usize,
    pub(super) blocked_for_us: u64,
    pub(super) block_events: u64,
    pub(super) total_blocked_us: u64,
}

impl GraphInner {
    pub(super) fn begin_watermark_block(&self, edge: EdgeId) {
        let Some(event) = self.edges[edge]
            .watermark_backpressure
            .enter(self.epoch_us())
        else {
            return;
        };
        if event.is_power_of_two() {
            runtime::log_warn(&format!(
                "global watermark backpressure #{event}: graph input `{}` blocked \
                 (queued={}, limit={}); waiting for downstream queues and pollers to drain",
                self.edges[edge].name,
                self.shared.total_queued(),
                self.shared.config.max_queued_packets,
            ));
        }
    }

    pub(super) fn finish_watermark_block(&self, edge: EdgeId, log_recovery: bool) {
        let Some((event, elapsed)) = self.edges[edge]
            .watermark_backpressure
            .leave(self.epoch_us())
        else {
            return;
        };
        if log_recovery && event.is_power_of_two() {
            runtime::log_info(&format!(
                "global watermark backpressure #{event} cleared: graph input `{}` resumed after \
                 {elapsed}us (queued={}, limit={})",
                self.edges[edge].name,
                self.shared.total_queued(),
                self.shared.config.max_queued_packets,
            ));
        }
    }

    /// `workers_idle` 只是执行器快照,不能直接等同于图没有可推进工作:
    /// 队列入队、blocked staging 恢复与下游调度通知可能交错,使任务队列暂时为空。
    /// 先全图重扫就绪性并重试刷新;若活动代数仍不变且 worker 仍空闲,才算稳定。
    pub(super) fn retry_idle_progress(&self) -> bool {
        let before = self.activity_gen();
        for node in 0..self.nodes.len() {
            self.schedule_node(node);
        }
        self.resume_blocked_flushes();
        while self.pump_step() {}
        !self.workers_idle() || self.activity_gen() != before
    }

    /// 驱动按序刷新。下游容量不足时保留槽与 staging,让出 worker;由下游出队后重试。
    ///
    /// ⚠ 本文件里对 `blocked_flush_nodes` 的每次增删都**刻意放在持 `node.sched` 的作用域内**:
    /// 它是「哪些节点有 `blocked_flush`」的索引,与 `sched.blocked_flush` 必须原子地一起变。
    /// 分两步做会露出「索引已空、节点仍有 blocked_flush」的中间态,而 `wait_done` / `is_idle`
    /// 正是读这个索引判死锁的 —— 读到中间态就把正常排空误报成卡死。
    /// 由此锁序恒为 `node.sched` → `blocked_flush_nodes`(见 design.md R2),反向即死锁。
    pub(super) fn drive_invocation_flushes(&self, n: NodeId, mut first: Option<(usize, bool)>) {
        let node = &self.nodes[n];
        loop {
            let item = match first.take() {
                Some(value) => Some(value),
                None => {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    if let Some(BlockedFlush::Invocation { slot, ok }) = sched.blocked_flush.take()
                    {
                        Some((slot, ok))
                    } else {
                        let next = sched.next_flush_seq;
                        match sched.pending_flush.remove(&next) {
                            Some(value) => Some(value),
                            None => {
                                sched.flushing = false;
                                self.blocked_flush_nodes
                                    .lock()
                                    .expect("blocked flush lock poisoned")
                                    .remove(&n);
                                None
                            }
                        }
                    }
                }
            };
            let Some((slot, ok)) = item else {
                return;
            };

            if ok && !self.shared.is_cancelled() && !self.shared.has_error() {
                match self.flush_staging(n, slot) {
                    Ok(true) => {}
                    Ok(false) => {
                        let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                        sched.blocked_flush = Some(BlockedFlush::Invocation { slot, ok });
                        sched.flushing = false;
                        self.blocked_flush_nodes
                            .lock()
                            .expect("blocked flush lock poisoned")
                            .insert(n);
                        return;
                    }
                    Err(error) => {
                        unsafe { node.ctx_slot(slot) }.discard_staging();
                        self.shared.record_error(error);
                    }
                }
            } else {
                unsafe { node.ctx_slot(slot) }.discard_staging();
            }
            unsafe { node.ctx_slot(slot) }.clear_inputs();
            {
                let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                sched.next_flush_seq += 1;
                sched.in_flight -= 1;
                sched.free_slots.push(slot);
            }
        }
    }

    pub(super) fn resume_blocked_flushes(&self) {
        if self.shared.is_cancelled() || self.shared.has_error() {
            self.finish_all_backpressure_blocks();
        }
        // 取快照即释放索引锁 —— 下面要锁 `node.sched`,而锁序是 sched → 索引(R2),
        // 反过来就死锁。这里靠的是「临时 MutexGuard 活到语句末」:别把 `.lock()` 的
        // 结果绑成局部变量,那会把它的存活期拉长到覆盖下面的 sched 加锁。
        let blocked: Vec<NodeId> = self
            .blocked_flush_nodes
            .lock()
            .expect("blocked flush lock poisoned")
            .iter()
            .copied()
            .collect();
        for node_id in blocked {
            let blocked = {
                let mut sched = self.nodes[node_id]
                    .sched
                    .lock()
                    .expect("scheduler lock poisoned");
                if sched.flushing || sched.blocked_flush.is_none() {
                    None
                } else {
                    sched.flushing = true;
                    sched.blocked_flush
                }
            };
            match blocked {
                Some(BlockedFlush::Invocation { .. }) => {
                    self.drive_invocation_flushes(node_id, None);
                    self.finish(node_id);
                }
                Some(BlockedFlush::Close) => {
                    self.resume_blocked_close(node_id);
                }
                None => {}
            }
        }
    }

    pub(super) fn resume_blocked_close(&self, n: NodeId) {
        let node = &self.nodes[n];
        if self.shared.is_cancelled() || self.shared.has_error() {
            unsafe { node.ctx_slot(0) }.discard_staging();
            unsafe { node.ctx_slot(0) }.clear_inputs();
            {
                let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                sched.blocked_flush = None;
                sched.flushing = false;
                sched.closed = true;
                self.blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .remove(&n);
            }
            for &edge in &node.outputs {
                self.close_edge(edge);
            }
            self.notify_activity();
            return;
        }
        match self.flush_staging(n, 0) {
            Ok(true) => {
                unsafe { node.ctx_slot(0) }.clear_inputs();
                {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    sched.blocked_flush = None;
                    sched.flushing = false;
                    sched.closed = true;
                    self.blocked_flush_nodes
                        .lock()
                        .expect("blocked flush lock poisoned")
                        .remove(&n);
                }
                for &edge in &node.outputs {
                    self.close_edge(edge);
                }
                self.notify_activity();
            }
            Ok(false) => {
                node.sched.lock().expect("scheduler lock poisoned").flushing = false;
            }
            Err(error) => {
                unsafe { node.ctx_slot(0) }.discard_staging();
                unsafe { node.ctx_slot(0) }.clear_inputs();
                self.shared.record_error(error);
                {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    sched.blocked_flush = None;
                    sched.flushing = false;
                    sched.closed = true;
                    self.blocked_flush_nodes
                        .lock()
                        .expect("blocked flush lock poisoned")
                        .remove(&n);
                }
                for &edge in &node.outputs {
                    self.close_edge(edge);
                }
                self.notify_activity();
            }
        }
    }

    /// 把某个槽暂存区的输出分发到下游(此时不持有任何算子回调栈)。
    pub(super) fn flush_staging(&self, n: NodeId, slot: usize) -> Result<bool> {
        let node = &self.nodes[n];
        let input_ts = unsafe { node.ctx_slot(slot) }.input_ts;
        let reservations = {
            let ctx = unsafe { node.ctx_slot(slot) };
            let outputs: Vec<(EdgeId, usize)> = node
                .outputs
                .iter()
                .copied()
                .zip(ctx.staging.iter())
                .map(|(edge, packets)| (edge, packets.len()))
                .collect();
            let Some(reservations) = self.reserve_internal_capacity(n, &outputs)? else {
                return Ok(false);
            };
            reservations
        };
        // 逐口处理,不再先 `collect` 成一个临时 `Vec<OutputBatch>`(perf 显示那个临时
        // Vec 连带 malloc/free 可观)。仍然**不在调用 `dispatch` 时持有 `&mut Context`** ——
        // 那是本函数原有的安全性质(避免与回调期交出的 `*mut Context` 形成别名),保留。
        for i in 0..node.outputs.len() {
            let edge = node.outputs[i];
            let (mut packets, explicit_bound) = {
                let ctx = unsafe { node.ctx_slot(slot) };
                (
                    std::mem::take(&mut ctx.staging[i]),
                    ctx.next_bounds[i].take(),
                )
            };
            self.flush_one(n, edge, &packets, explicit_bound, input_ts);
            // 归还缓冲:清空后放回 staging,容量得以复用 —— 否则下次产出要重新分配。
            packets.clear();
            unsafe { node.ctx_slot(slot) }.staging[i] = packets;
        }
        self.release_internal_reservations(&reservations);
        // 真正入队后 reservation 已转化为 queue len / bytes；此刻重试其它被挡住的刷新。
        self.resume_blocked_flushes();
        Ok(true)
    }

    pub(super) fn reserve_internal_capacity(
        &self,
        producer: NodeId,
        outputs: &[(EdgeId, usize)],
    ) -> Result<Option<Vec<InputQueueReservation>>> {
        let mut reservations = Vec::new();
        for &(edge, count) in outputs {
            if count == 0 {
                continue;
            }
            for &(consumer, port) in &self.edges[edge].consumers {
                let node = &self.nodes[consumer];
                if node.input_is_back_edge[port]
                    || matches!(node.policy, InputPolicy::FixedSize { .. })
                {
                    continue;
                }
                let Some(capacity) = node.input_queue_capacity[port] else {
                    continue;
                };
                if count > capacity {
                    self.release_internal_reservations(&reservations);
                    return Err(Error::Kernel(format!(
                        "node `{}` emits a batch of {count} packets to edge `{}`, exceeding consumer \
                         `{}` input port `{}` effective packet capacity {capacity}; set \
                         `input_queues.packets` or `input_queues.ports.{}.packets` to at least {count}, \
                         or emit smaller batches",
                        self.nodes[producer].name,
                        self.edges[edge].name,
                        node.name,
                        node.in_ports.name(port).unwrap_or("?"),
                        node.in_ports.name(port).unwrap_or("?"),
                    )));
                }
                // queue len 与 reservation 必须在同一把 queue 锁下观察/更新。
                // 否则可能读到旧 len，恰逢另一刷新已入队并释放 reservation，
                // 两个刷新都以为有空位而共同越过容量。
                let queue = node.input_queues[port].lock().expect("queue lock poisoned");
                let queued = queue.len();
                let reserved = node.input_queue_reserved[port].load(Ordering::SeqCst);
                let packets_full = queued + reserved + count > capacity;
                if packets_full {
                    drop(queue);
                    self.mark_input_queue_blocked(InputQueueBlockContext {
                        producer,
                        consumer,
                        port,
                        capacity,
                        queued,
                        reserved,
                        incoming: count,
                    });
                    self.release_internal_reservations(&reservations);
                    return Ok(None);
                }
                node.input_queue_reserved[port].fetch_add(count, Ordering::SeqCst);
                reservations.push(InputQueueReservation {
                    node: consumer,
                    port,
                    packets: count,
                });
                drop(queue);
                self.finish_input_queue_block(consumer, port, true);
            }
        }
        Ok(Some(reservations))
    }

    pub(super) fn release_internal_reservations(&self, reservations: &[InputQueueReservation]) {
        for reservation in reservations {
            self.nodes[reservation.node].input_queue_reserved[reservation.port]
                .fetch_sub(reservation.packets, Ordering::SeqCst);
        }
    }

    pub(super) fn epoch_us(&self) -> i64 {
        self.epoch.elapsed().as_micros().min(i64::MAX as u128) as i64
    }

    pub(super) fn mark_input_queue_blocked(&self, context: InputQueueBlockContext) {
        let InputQueueBlockContext {
            producer,
            consumer,
            port,
            capacity,
            queued,
            reserved,
            incoming,
        } = context;
        let stats = &self.nodes[consumer].input_queue_stats[port];
        let since = self.epoch_us().saturating_add(1);
        if stats
            .blocked_since_us
            .compare_exchange(0, since, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            let event = stats.block_events.fetch_add(1, Ordering::Relaxed) + 1;
            if event.is_power_of_two() {
                runtime::log_warn(&format!(
                    "internal backpressure #{event}: producer `{}` paused by consumer `{}` input \
                     `{}` (capacity={capacity} packets, queued={queued}, reserved={reserved}, \
                     incoming={incoming}); the worker was released and will resume after dequeue",
                    self.nodes[producer].name,
                    self.nodes[consumer].name,
                    self.nodes[consumer].in_ports.name(port).unwrap_or("?"),
                ));
            }
        }
    }

    pub(super) fn finish_input_queue_block(&self, node: NodeId, port: usize, log_recovery: bool) {
        let stats = &self.nodes[node].input_queue_stats[port];
        let since = stats.blocked_since_us.swap(0, Ordering::SeqCst);
        if since != 0 {
            let elapsed = self
                .epoch_us()
                .saturating_sub(since.saturating_sub(1))
                .max(0) as u64;
            stats.blocked_total_us.fetch_add(elapsed, Ordering::Relaxed);
            let event = stats.block_events.load(Ordering::Relaxed);
            if log_recovery && event.is_power_of_two() {
                let producer = self.edges[self.nodes[node].inputs[port]]
                    .producer
                    .map(|producer| self.nodes[producer].name.as_str())
                    .unwrap_or("<graph input>");
                runtime::log_info(&format!(
                    "internal backpressure #{event} cleared: producer `{producer}` resumed after \
                     consumer `{}` input `{}` drained (blocked {elapsed}us)",
                    self.nodes[node].name,
                    self.nodes[node].in_ports.name(port).unwrap_or("?"),
                ));
            }
        }
    }

    pub(super) fn finish_all_backpressure_blocks(&self) {
        for node in 0..self.nodes.len() {
            for port in 0..self.nodes[node].input_queues.len() {
                self.finish_input_queue_block(node, port, false);
            }
        }
    }

    pub(super) fn backpressure_stall_details(&self, producers: &[NodeId]) -> Vec<String> {
        let producer_set: BTreeSet<NodeId> = producers.iter().copied().collect();
        let mut details = Vec::new();
        for (consumer, node) in self.nodes.iter().enumerate() {
            for port in 0..node.input_queues.len() {
                let edge = node.inputs[port];
                let Some(producer) = self.edges[edge].producer else {
                    continue;
                };
                if !producer_set.contains(&producer) {
                    continue;
                }
                let Some(queue) = self.input_queue_stats(consumer, port) else {
                    continue;
                };
                if !queue.blocked {
                    continue;
                }
                let capacity = queue
                    .packet_capacity
                    .map_or_else(|| "unbounded".to_string(), |value| value.to_string());
                details.push(format!(
                    "{} -> {}.{}(capacity={}, queued={}, reserved={}, blocked={}us)",
                    self.nodes[producer].name,
                    queue.node_name,
                    queue.port_name,
                    capacity,
                    queue.queued_packets,
                    queue.reserved_packets,
                    queue.blocked_for_us,
                ));
            }
        }
        details
    }

    /// `flush_staging` 的单个输出口分支(拆出来只为让上面的循环短一点,逻辑未变)。
    pub(super) fn flush_one(
        &self,
        n: NodeId,
        edge: EdgeId,
        packets: &[Packet],
        explicit_bound: Option<Timestamp>,
        input_ts: Timestamp,
    ) {
        if !packets.is_empty() {
            if self.basic_stats() {
                self.nodes[n]
                    .stats
                    .packets_out
                    .fetch_add(packets.len() as u64, Ordering::Relaxed);
            }
            self.dispatch(edge, packets);
            self.schedule_consumers(edge);
            return;
        }
        // **没有产出时也必须推进下游边界**,否则下游会永远等这一路。
        // 这是自动的:算子不显式调 SetNextTimestampBound 也不会卡住管线
        // (Filter 这类会丢包的算子因此不必自己操心)。
        let bound = match explicit_bound {
            Some(b) => b,
            None if input_ts.is_allowed_in_stream() => input_ts.next_allowed_in_stream(),
            None => return,
        };
        self.propagate_bound(edge, bound);
    }
}
