use super::scheduler::KernelPhase;
use super::*;

// Graph start, shutdown, reset, and wait state machine.

impl Graph {
    pub fn start(&self) -> Result<()> {
        self.inner.start()
    }

    pub fn reset(&self) -> Result<()> {
        self.inner.reset()
    }

    pub fn wait_done(&self) -> Result<()> {
        self.inner.wait_done(None)
    }

    pub fn wait_until_idle(&self) -> Result<()> {
        self.inner.wait_until_idle(None)
    }
}

impl GraphInner {
    /// 一次调用完成后:尽力再填满容量(并行调度多个 in-flight),并尝试关闭。
    pub(super) fn finish(&self, n: NodeId) {
        self.schedule_source_resumption(n);
        self.schedule_node(n);
        self.maybe_close(n);
    }

    /// 关流推进:所有输入已关且排空 → close 算子 → 关自己的输出边 → 递归下游。
    pub(super) fn maybe_close(&self, n: NodeId) -> bool {
        let node = &self.nodes[n];
        let force = self.shared.has_error() || self.shared.is_cancelled();

        // 在锁下**认领**关流:并发时(宿主线程与工作线程可能同时到这里)
        // 只有一个线程能置位 close_started,从而保证算子的 Close 只被调用一次。
        // in_flight != 0 表示还有并行调用在跑或等待刷新 —— 必须全部落地才能关。
        {
            let mut s = node.sched.lock().expect("scheduler lock poisoned");
            if s.close_started || s.in_flight != 0 || !s.opened {
                return false;
            }
            if !force && !node.all_inputs_closed_and_drained() {
                return false;
            }
            s.close_started = true;
        }

        // 此刻 in_flight==0,所有槽空闲;Close 是串行的,用槽 0。
        {
            let ctx = unsafe { node.ctx_slot(0) };
            ctx.reset();
            ctx.close_reason = self.shared.close_reason();
            ctx.input_ts = Timestamp::done();
        }
        let rc = self.call_kernel(n, 0, KernelPhase::Close);
        if rc != 0 {
            let ctx = unsafe { node.ctx_slot(0) };
            let e = ctx.take_error(rc);
            ctx.discard_staging();
            self.shared.record_error(e);
        } else if let Err(e) = self.check_output_types(n, 0) {
            unsafe { node.ctx_slot(0) }.discard_staging();
            self.shared.record_error(e);
        } else {
            match self.flush_staging(n, 0) {
                Ok(true) => {}
                Ok(false) => {
                    let mut sched = node.sched.lock().expect("scheduler lock poisoned");
                    sched.blocked_flush = Some(BlockedFlush::Close);
                    sched.flushing = false;
                    drop(sched);
                    self.blocked_flush_nodes
                        .lock()
                        .expect("blocked flush lock poisoned")
                        .insert(n);
                    return false;
                }
                Err(error) => {
                    unsafe { node.ctx_slot(0) }.discard_staging();
                    self.shared.record_error(error);
                }
            }
        }
        unsafe { node.ctx_slot(0) }.clear_inputs();
        node.sched.lock().expect("scheduler lock poisoned").closed = true;

        for &e in &node.outputs {
            self.close_edge(e);
        }
        self.notify_activity();
        true
    }

    pub(super) fn close_edge(&self, edge: EdgeId) {
        let e = &self.edges[edge];
        if e.closed.swap(true, Ordering::SeqCst) {
            return; // 已关
        }
        for &(node, port) in &e.consumers {
            self.nodes[node].input_closed[port].store(true, Ordering::SeqCst);
            // 关闭即「永远不会再有数据」,边界直接到 Done,让下游不必再等这一路
            self.nodes[node].advance_bound(port, Timestamp::done());
            // 关流会改变就绪判定(空口不再阻塞对齐),必须重扫
            self.schedule_node(node);
        }
        // 该边的 poller 在队列排空后即视为结束
        for p in e.pollers.lock().expect("poller list lock poisoned").iter() {
            p.closed.store(true, Ordering::SeqCst);
        }
    }

    pub(super) fn set_state_draining_if_all_inputs_closed(&self) {
        let all = self.graph_inputs.iter().all(|&e| self.edges[e].is_closed());
        if all {
            let mut st = self.state.lock().expect("state lock poisoned");
            if *st == State::Running {
                *st = State::Draining;
            }
        }
    }

    /// 尝试推进任一节点的关流;返回是否有进展。
    pub(super) fn try_advance_closing(&self) -> bool {
        let mut progressed = false;
        for n in 0..self.nodes.len() {
            if self.maybe_close(n) {
                progressed = true;
            }
        }
        if !progressed && self.all_nodes_closed() {
            self.set_state(State::Terminated);
        }
        progressed
    }

    pub(super) fn all_nodes_closed(&self) -> bool {
        self.nodes
            .iter()
            .all(|n| n.sched.lock().expect("scheduler lock poisoned").closed)
    }

    /// 距 deadline 还剩多久;`None` 表示已超时。无 deadline 时返回一个固定的
    /// 轮询上限,以免因通知丢失而永久挂住。
    pub(super) fn remaining(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> Option<std::time::Duration> {
        const POLL_CAP: std::time::Duration = std::time::Duration::from_millis(50);
        match deadline {
            None => Some(POLL_CAP),
            Some(d) => {
                let now = std::time::Instant::now();
                if now >= d {
                    None
                } else {
                    Some((d - now).min(POLL_CAP))
                }
            }
        }
    }
    pub(super) fn start(self: &Arc<Self>) -> Result<()> {
        let st = self.state();
        if st != State::Initialized {
            return Err(Error::State(format!(
                "start can only be called in Initialized (current {st:?})"
            )));
        }

        // 校验算子声明的必需 side packet
        let provided = self.side_packets.lock().expect("side packet lock poisoned");
        for (need, who) in &self.required_side_packets {
            if !provided.contains_key(need) {
                return Err(Error::InvalidArg(format!(
                    "missing required side packet `{need}` -- node `{who}` declared it in GetContract; \
                     inject it with set_side_packet before start"
                )));
            }
        }
        let sp = Arc::new(provided.clone());
        drop(provided);

        // 把 side packets 灌进各节点的**所有** context 槽,然后 open(用槽 0,串行)。
        // reset 重跑时算子实例被保留、`opened` 仍为 true —— 那种情况下**跳过 open**
        // (不重跑 open 正是 reset 省重载模型的价值),只重灌 side packet + 复位槽。
        for (i, node) in self.nodes.iter().enumerate() {
            let already_open = node.sched.lock().expect("scheduler lock poisoned").opened;
            for slot in 0..node.max_in_flight {
                // 安全性:尚未开始调度,所有槽空闲,可独占写入。
                let ctx = unsafe { node.ctx_slot(slot) };
                ctx.side_packets = sp.clone();
                ctx.reset();
                ctx.input_ts = Timestamp::unstarted();
            }
            if already_open {
                continue; // reset 后重跑:算子实例与其 open 状态都保留,不再 open
            }
            let rc = self.call_kernel(i, 0, KernelPhase::Open);
            if rc != 0 {
                let e = unsafe { node.ctx_slot(0) }.take_error(rc);
                self.shared.record_error(e.clone());
                return Err(e);
            }
            node.sched.lock().expect("scheduler lock poisoned").opened = true;
        }

        self.set_state(State::Running);
        self.run_started_us
            .store(self.epoch_us().saturating_add(1), Ordering::Relaxed);
        *self
            .dot_intervals
            .lock()
            .expect("DOT interval lock poisoned") = dot::DotIntervalBaselines::default();

        // 拉起真正有节点归属的执行器。必须在 Arc 存在之后:工作线程持 Weak,避免 Arc 环。
        let weak = Arc::downgrade(self);
        for (executor_id, executor) in self.executors.iter().enumerate() {
            if self.nodes.iter().any(|node| node.executor == executor_id) {
                executor.start(weak.clone());
            }
        }
        // 源节点(0 输入)无输入触发,须在此显式起调度 —— start 里唯一主动调度的一处。
        // 之后由 finish→schedule_node 自我续产,直到内核 source_done() 或图被 cancel。
        for i in 0..self.nodes.len() {
            if self.nodes[i].is_source() {
                self.schedule_node(i);
            }
        }
        Ok(())
    }

    /// 复位为可再次 `start` 的状态,**保留已 open 的算子实例**(省掉每会话重载模型的
    /// 开销)。字段的「保留 / 复位」分类见 docs/design.md §7.13。
    ///
    /// 前提:图必须**已静止** —— `Terminated` 且 `is_idle()`(没有 worker 还在算子里)。
    /// 否则返回 `Error::State`。宿主通常先 `wait_done()` 再 `reset()`。
    ///
    /// **不碰线程池**:worker 随图存活、此刻都 park 在 condvar 上、`stop` 仍为 false,
    /// 下一轮 `start` 直接复用(见 executor.rs 模块头);shutdown+join 只发生在 Drop。
    pub(super) fn reset(&self) -> Result<()> {
        // 1. 校验静止。in_flight==0 且委托队列为空 ⇒ 没有 worker 在 run_node 中途,
        //    故下面所有「无并发」的复位都成立(与 Drop / start 用同一条静止依据)。
        {
            let st = *self.state.lock().expect("state lock poisoned");
            if st != State::Terminated || !self.is_idle() {
                return Err(Error::State(
                    "reset requires the graph to be Terminated and idle; call wait_done() first"
                        .into(),
                ));
            }
        }

        // 2. 清 GraphShared:先清 error/cancelled,否则下一轮 start 的 try_claim 会被旧
        //    has_error 挡回(mod.rs try_claim 首判)。
        self.shared.reset_run_state();
        self.blocked_flush_nodes
            .lock()
            .expect("blocked flush lock poisoned")
            .clear();

        // 3. 逐 Edge 复位。last_sent 必须回 unset() —— 否则单调性校验会拒掉下一轮
        //    从图输入口发的第一个包(时间戳通常又从小开始)。
        for e in &self.edges {
            e.closed.store(false, Ordering::SeqCst);
            e.dropped.store(0, Ordering::Relaxed);
            e.watermark_backpressure.reset();
            *e.last_sent.lock().expect("last_sent lock poisoned") = Timestamp::unset();
            // poller / observer 是宿主持有、engine 存 Arc —— **保留**列表,只复位内容,
            // 让宿主复用同一个 Poller 句柄再取下一轮输出。
            for pl in e.pollers.lock().expect("poller list lock poisoned").iter() {
                pl.clear(self);
                pl.closed.store(false, Ordering::SeqCst);
                pl.dropped.store(0, Ordering::Relaxed);
                pl.block_backpressure.reset();
            }
        }

        // 4. 逐 Node 复位。
        for node in &self.nodes {
            // sched 整体重建(一把覆盖 next_seq / free_slots / ready / pending_flush 等全部
            // 运行态,不会漏),再单独把 opened 置回 true —— **保留 open 是 reset 的价值**。
            {
                let mut sc = node.sched.lock().expect("scheduler lock poisoned");
                let opened = sc.opened;
                *sc = NodeSched::new(node.max_in_flight);
                sc.opened = opened;
            }
            node.stats.reset();
            for q in &node.input_queues {
                q.lock().expect("queue lock poisoned").clear();
            }
            for bytes in &node.input_queue_bytes {
                bytes.store(0, Ordering::SeqCst);
            }
            for reserved in &node.input_queue_reserved {
                reserved.store(0, Ordering::SeqCst);
            }
            for stats in &node.input_queue_stats {
                stats.reset();
            }
            for c in &node.input_closed {
                c.store(false, Ordering::SeqCst);
            }
            // input_bounds 必须回 pre_stream()(不是上一轮 close 推到的 done())——
            // 否则 readiness/对齐会认为每个空口「已到流尾」,语义崩坏。
            for b in &node.input_bounds {
                *b.lock().expect("bound lock poisoned") = Timestamp::pre_stream();
            }
            node.source_done.store(false, Ordering::SeqCst);
            node.source_waiting.store(false, Ordering::SeqCst);
            node.source_wake_generation.fetch_add(1, Ordering::SeqCst);
            *node.last_fire.lock().expect("last_fire lock poisoned") = None;
            // 逐槽复位 Context:此刻 in_flight==0,与 start/Drop 同为「独占相」,无并发。
            for slot in 0..node.ctxs.len() {
                unsafe { node.ctx_slot(slot) }.reset();
            }
        }

        // 5. GraphInner 顶层。side_packets 保留(下一轮 start 会自动 clone 进各 ctx)。
        //    epoch 不动:它只是各诊断时间戳的单调基准。
        for executor in &self.executors {
            executor.reset_run_state();
        }
        self.in_flight.store(0, Ordering::SeqCst);
        self.delegated_cursor.store(0, Ordering::Relaxed);
        self.delegated_running.store(false, Ordering::Release);
        self.wakeup_pending.store(false, Ordering::Release);
        self.wakeup_generation.store(0, Ordering::Release);
        {
            let mut a = self.activity.0.lock().unwrap_or_else(|e| e.into_inner());
            a.waiters = 0;
        }
        self.paused.store(false, Ordering::SeqCst);
        self.run_started_us.store(0, Ordering::Relaxed);
        *self
            .dot_intervals
            .lock()
            .expect("DOT interval lock poisoned") = dot::DotIntervalBaselines::default();

        // 6. 最后置 state —— 前面的清理对「下一次 start」全部可见后,才对外表现为可 start。
        *self.state.lock().expect("state lock poisoned") = State::Initialized;
        Ok(())
    }

    /// 等待图跑完。`deadline` 为 `None` 表示不限时。
    ///
    /// 期间会**借用宿主线程**执行主线程任务(默认执行器,§7.9),
    /// 同时等待线程池里的任务完成。
    pub(super) fn wait_done(&self, deadline: Option<std::time::Instant>) -> Result<()> {
        loop {
            // 先把能自己干的干完
            while self.pump_step() {}
            if self.all_nodes_closed() && self.workers_idle() {
                break;
            }
            // 在判断是否空闲**之前**捕获活动代数,再据此等待 —— 否则会丢唤醒。
            let before = self.activity_gen();
            if self.workers_idle() {
                self.resume_blocked_flushes();
                let blocked: Vec<NodeId> = self
                    .blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .iter()
                    .copied()
                    .collect();
                if !blocked.is_empty() {
                    if self.retry_backpressure_progress() {
                        continue;
                    }
                    let details = self.backpressure_stall_details(&blocked);
                    return Err(Error::Kernel(format!(
                        "wait_done: internal backpressure cannot make progress; blocked queues: [{}]. \
                         increase the input queue packet capacity or inspect downstream alignment",
                        details.join("; ")
                    )));
                }
                // 空闲且未全关:再推一轮关流
                if self.try_advance_closing() {
                    continue;
                }
                // try_advance_closing 把最后一个节点关掉并置 Terminated 时会返回 false
                // (它不把“到达终态”算作推进)。此时图其实已跑完 —— 常见触发是:工作线程
                // 在本轮 all_nodes_closed() 判定与这里之间关掉了最后一个节点。必须重判,
                // 否则会把已完成的图误报成“卡住”(症状:未能关闭的节点列表为空 [])。
                if self.all_nodes_closed() {
                    break;
                }
                // 推不动了。这时**不能返回 Ok** —— 图并没有跑完。
                // 区分两种成因,给出可操作的报错而不是静默成功或永久挂住:
                let inputs_open: Vec<&str> = self
                    .graph_inputs
                    .iter()
                    .filter(|&&e| !self.edges[e].is_closed())
                    .map(|&e| self.edges[e].name.as_str())
                    .collect();
                if !inputs_open.is_empty() {
                    return Err(Error::State(format!(
                        "wait_done: graph input ports [{}] still open, the graph won't finish on its own -- \
                         call close_input/close_all_inputs first",
                        inputs_open.join(", ")
                    )));
                }
                match self.remaining(deadline) {
                    Some(duration) => self.wait_activity_since(before, duration),
                    None => return Err(Error::Timeout),
                }
                if self.activity_gen() != before || !self.workers_idle() {
                    continue;
                }
                if self.remaining(deadline).is_none() {
                    return Err(Error::Timeout);
                }
                let stuck: Vec<&str> = (0..self.nodes.len())
                    .filter(|&n| {
                        !self.nodes[n]
                            .sched
                            .lock()
                            .expect("scheduler lock poisoned")
                            .closed
                    })
                    .map(|n| self.nodes[n].name.as_str())
                    .collect();
                return Err(Error::Kernel(format!(
                    "wait_done: all inputs closed but the graph is still idle, nodes not closed: [{}]. \
                     usually some kernel's output/close condition is unmet (use dump to inspect queue backlog)",
                    stuck.join(", ")
                )));
            }
            // 线程池还在跑:等它有进展(相对刚才捕获的 before)
            match self.remaining(deadline) {
                Some(d) => {
                    self.wait_activity_since(before, d);
                }
                None => return Err(Error::Timeout),
            }
        }
        if self.all_nodes_closed() {
            self.set_state(State::Terminated);
        }
        if self.shared.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match self.shared.first_error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 等到在途任务都处理完(但不结束图)。
    pub(super) fn wait_until_idle(&self, deadline: Option<std::time::Instant>) -> Result<()> {
        loop {
            while self.run_one_main_task() {}
            self.resume_blocked_flushes();
            // 判断空闲**之前**捕获代数,防止丢唤醒(见 activity_gen)。
            let before = self.activity_gen();
            if self.is_idle() {
                break;
            }
            if self.workers_idle() {
                let blocked: Vec<NodeId> = self
                    .blocked_flush_nodes
                    .lock()
                    .expect("blocked flush lock poisoned")
                    .iter()
                    .copied()
                    .collect();
                if !blocked.is_empty() && self.retry_backpressure_progress() {
                    continue;
                }
                if self.is_idle() {
                    break;
                }
                if !blocked.is_empty() {
                    let details = self.backpressure_stall_details(&blocked);
                    return Err(Error::Kernel(format!(
                        "wait_until_idle: internal backpressure cannot make progress; blocked queues: [{}]",
                        details.join("; ")
                    )));
                }
            }
            match self.remaining(deadline) {
                Some(d) => {
                    self.wait_activity_since(before, d);
                }
                None => return Err(Error::Timeout),
            }
        }
        match self.shared.first_error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for GraphInner {
    /// 兜底关流:图被直接丢弃(没走 wait_done)时,已 open 的算子仍必须收到 Close,
    /// 否则算子里申请的资源(文件、连接、GPU 上下文)不会被释放。
    fn drop(&mut self) {
        // 先关停线程池并 join:否则工作线程可能触碰正在析构的节点。
        self.shutdown_executors();

        for n in 0..self.nodes.len() {
            let need_close = {
                let s = self.nodes[n].sched.lock().expect("scheduler lock poisoned");
                s.opened && !s.close_started
            };
            if !need_close {
                continue;
            }
            {
                // 安全性:线程池已 join,此刻只有 drop 这一条执行流,独占成立。用槽 0。
                let ctx = unsafe { self.nodes[n].ctx_slot(0) };
                ctx.reset();
                ctx.close_reason = self.shared.close_reason();
                ctx.input_ts = Timestamp::done();
            }
            let rc = self.call_kernel(n, 0, KernelPhase::Close);
            if rc != 0 {
                runtime::log_warn(&format!(
                    "node `{}`: close returned {rc} during graph destruction (ignored)",
                    self.nodes[n].name
                ));
            }
            unsafe { self.nodes[n].ctx_slot(0) }.clear_inputs();
            self.nodes[n]
                .sched
                .lock()
                .expect("scheduler lock poisoned")
                .closed = true;
        }
    }
}
