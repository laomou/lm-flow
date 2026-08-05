//! Node claiming, dispatch, execution, and ordered output flushing.

use super::*;

impl GraphInner {
    pub(super) fn state(&self) -> State {
        *self.state.lock().expect("state lock poisoned")
    }
    pub(super) fn set_state(&self, s: State) {
        *self.state.lock().expect("state lock poisoned") = s;
    }

    pub(super) fn send(&self, edge: EdgeId, pkt: Packet, blocking: bool) -> Result<()> {
        match self.state() {
            State::Running => {}
            State::Draining | State::Terminated => return Err(Error::Closed),
            s => {
                return Err(Error::State(format!(
                    "send requires the graph to be Running (current {s:?}); call start first"
                )))
            }
        }
        if self.shared.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(e) = self.shared.first_error() {
            return Err(e);
        }
        if self.edges[edge].is_closed() {
            return Err(Error::Closed);
        }
        // 图输入口上必须有明确时间戳
        if pkt.timestamp() == Timestamp::unset() {
            return Err(Error::InvalidArg(
                "packets on a graph input port must carry an explicit timestamp (UNSET is invalid)"
                    .into(),
            ));
        }

        // 全局水位:超限时把压力转化成图输入口背压(§7.5)。
        let mut watermark_blocked = false;
        while self.shared.over_watermark() {
            if !blocking {
                return Err(Error::WouldBlock);
            }
            if !watermark_blocked {
                watermark_blocked = true;
                self.begin_watermark_block(edge);
            }
            // 长时间背压等待期间图可能被取消/出错,及时退出而不是傻等。
            if self.shared.is_cancelled() {
                self.finish_watermark_block(edge, false);
                return Err(Error::Cancelled);
            }
            if let Some(e) = self.shared.first_error() {
                self.finish_watermark_block(edge, false);
                return Err(e);
            }
            // 先记活动代数,再尝试推进/等待 —— 避免判定与等待之间丢唤醒。
            let before = self.activity_gen();
            if self.pump_step() {
                continue; // 在调用线程上推进了主线程执行器
            }
            // 本线程推不动。若线程池还有在飞任务,就等它们排水(这才是真正的背压);
            // 若全图都空了水位却下不去(如下游无人消费),那是真卡死 —— 报错而非永久阻塞。
            if self.workers_idle() {
                self.finish_watermark_block(edge, false);
                return Err(Error::WouldBlock);
            }
            self.wait_activity_since(before, std::time::Duration::from_millis(100));
        }
        if watermark_blocked {
            self.finish_watermark_block(edge, true);
        }

        // 时间戳单调性:图输入口强制校验(ADR #23)
        self.check_input_monotonic(edge, &pkt)?;

        // 分发给该边的所有消费者(各自一份引用)与 poller/observer
        self.dispatch(edge, std::slice::from_ref(&pkt)); // 单包不必为它分配 Vec
        self.schedule_consumers(edge);
        Ok(())
    }

    /// 图输入口的时间戳必须严格递增(ADR #23)。
    ///
    /// 参照值单独记录在边上,而不是看队列里剩什么 —— 否则队列一排空,
    /// 回退甚至重复的时间戳就能悄悄混进来,下游行为随之变得难以解释。
    pub(super) fn check_input_monotonic(&self, edge: EdgeId, pkt: &Packet) -> Result<()> {
        let e = &self.edges[edge];
        let mut last = e.last_sent.lock().expect("timestamp lock poisoned");
        if *last != Timestamp::unset() && pkt.timestamp() <= *last {
            return Err(Error::InvalidArg(format!(
                "graph input port `{}` timestamps must be strictly increasing: previous {}, this one {}",
                e.name,
                *last,
                pkt.timestamp()
            )));
        }
        *last = pkt.timestamp();
        Ok(())
    }

    /// 把一批包投递到边的消费者与订阅者。
    /// 把一批包投递到边的每个消费者队列。**只读 `packets`**(逐个 `clone` 引用计数),
    /// 故取切片而非 `Vec` —— 让调用方保留缓冲的所有权与容量。
    pub(super) fn dispatch(&self, edge_id: EdgeId, packets: &[Packet]) {
        let edge = &self.edges[edge_id];

        // 订阅者(poller / observer)各自独立一份
        {
            let pollers = edge
                .pollers
                .lock()
                .expect("poller list lock poisoned")
                .clone();
            let mut any = false;
            for p in &pollers {
                for pkt in packets {
                    any |= p.push(self, pkt.clone());
                }
            }
            if any {
                self.notify_activity();
            }
        }
        {
            // 快照订阅者后**释放锁再回调** —— 回调是宿主代码(可能慢、可能回调进引擎),
            // 持锁调用会造成争用甚至重入死锁(observer 若又触达同一条边的 observers 锁)。
            // observer 只增不删,快照是安全的。
            let observers: Vec<Observer> = {
                let guard = edge.observers.lock().expect("observer list lock poisoned");
                if guard.is_empty() {
                    Vec::new()
                } else {
                    guard.clone()
                }
            };
            for o in &observers {
                for pkt in packets {
                    match o {
                        Observer::C { cb, user } => {
                            let ffi = crate::ffi::borrow_packet(pkt);
                            unsafe { cb(*user, ffi) };
                        }
                        Observer::Rust(f) => f(pkt),
                    }
                }
            }
        }

        // 内部消费者:每个输入口一份(仅克隆引用计数)
        for &(node, port) in &edge.consumers {
            let cap = if self.nodes[node].input_is_back_edge[port] {
                Some(1) // 反馈寄存器:cap-1 drop-old,只留最新一包
            } else {
                match &self.nodes[node].policy {
                    InputPolicy::FixedSize { capacity } => Some(*capacity),
                    _ => None,
                }
            };
            let mut dropped = 0u64;
            let mut q = self.nodes[node].input_queues[port]
                .lock()
                .expect("queue lock poisoned");
            for pkt in packets {
                // fixed_size:满则丢最旧的。这是**有意的有损**策略,且不阻塞上游,
                // 故与「内部边不背压」不冲突,而是其配套的内存约束手段。
                if let Some(cap) = cap {
                    while q.len() >= cap {
                        if let Some(old) = q.pop_front() {
                            self.nodes[node].input_queue_bytes[port]
                                .fetch_sub(old.byte_size(), Ordering::SeqCst);
                            self.shared.on_dequeue(old.byte_size());
                            dropped += 1;
                        } else {
                            break;
                        }
                    }
                }
                self.shared.on_enqueue(pkt.byte_size());
                self.nodes[node].input_queue_bytes[port]
                    .fetch_add(pkt.byte_size(), Ordering::SeqCst);
                q.push_back(pkt.clone());
            }
            let depth = q.len();
            let queued_bytes = self.nodes[node].input_queue_bytes[port].load(Ordering::SeqCst);
            self.nodes[node].input_queue_stats[port]
                .peak_packets
                .fetch_max(depth, Ordering::Relaxed);
            self.nodes[node].input_queue_stats[port]
                .peak_bytes
                .fetch_max(queued_bytes, Ordering::Relaxed);
            // 高水位:depth 本就为软限告警算好了,这里顺手 fetch_max —— 定位积压节点。
            self.nodes[node]
                .stats
                .peak_queue_depth
                .fetch_max(depth, Ordering::Relaxed);
            drop(q);
            // 入队后,该口不会再来 <= 最后这个包时间戳的数据
            if let Some(last) = packets.last() {
                self.nodes[node].advance_bound(port, last.timestamp().next_allowed_in_stream());
            }
            if dropped > 0 {
                self.note_dropped(edge_id, dropped);
            }
            self.warn_if_over_soft_limit(edge_id, depth);
        }
    }

    /// 记录丢包。**绝不静默**:首次丢弃打 WARN,之后按指数退避,避免日志洪水。
    pub(super) fn note_dropped(&self, edge_id: EdgeId, n: u64) {
        let e = &self.edges[edge_id];
        let before = e.dropped.fetch_add(n, Ordering::Relaxed); // 纯计数器
        let after = before + n;
        if before == 0 || after.is_power_of_two() {
            runtime::log_warn(&format!(
                "edge `{}` has dropped {} packets total due to the fixed_size policy (consumer can't keep up; observe with dropped_count)",
                e.name, after
            ));
        }
    }

    /// 内部边只有软水位:超了告警,但**不阻塞生产者**(§7.5)。
    pub(super) fn warn_if_over_soft_limit(&self, edge_id: EdgeId, depth: usize) {
        let limit = self.shared.config.max_queue_size;
        if limit == 0 || depth <= limit {
            return;
        }
        // 指数退避,避免日志洪水:depth 恰为 limit 的 2^k 倍时才打
        let ratio = depth / limit;
        if ratio.is_power_of_two() {
            runtime::log_warn(&format!(
                "edge `{}` has {} packets backlogged (soft limit {}); consumer may not be keeping up",
                self.edges[edge_id].name, depth, limit
            ));
        }
    }

    pub(super) fn schedule_consumers(&self, edge: EdgeId) {
        let consumers: Vec<NodeId> = self.edges[edge].consumers.iter().map(|&(n, _)| n).collect();
        for n in consumers {
            self.schedule_node(n);
        }
    }

    /// 把一个已认领的调用派给节点所属的执行器。
    /// 与 `try_claim` 1:1 配对(每次成功认领派一个任务)。
    ///
    /// 全局 `in_flight` 已在认领仍持节点调度锁时递增,这里不能再加。否则
    /// `try_claim` 返回到任务真正入队之间会出现「节点已有已认领调用、全局仍显示空闲」
    /// 的窗口,`wait_done` 可能把正常排空误报成卡死。
    pub(super) fn dispatch_task(&self, n: NodeId) {
        if !self.executors[self.nodes[n].executor].submit(n) {
            // 池已关停(仅发生在拆图时):撤销全局计数。该次认领残留在 ready 里,
            // 但拆图路径不依赖精确排空(GraphInner::drop 兜底关流),不会死锁。
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
        self.notify_activity();
    }

    /// 线程池工作线程的入口。
    pub fn run_node_on_worker(&self, n: NodeId) {
        self.run_node(n);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if self.shared.is_cancelled() || self.shared.has_error() {
            self.resume_blocked_flushes();
            self.finish_all_backpressure_blocks();
        }
        self.notify_activity();
    }

    /// 执行器空闲 = 没有已认领调用,且没有委托给宿主线程的待办。
    ///
    /// `in_flight` 在 `try_claim` 内、仍持节点调度锁时递增,覆盖「已认领但尚未入队」
    /// 的阶段;否则并发关流可能撞上瞬时假空闲。
    pub(super) fn workers_idle(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0
            && self
                .executors
                .iter()
                .all(|executor| !executor.is_delegating() || executor.pending() == 0)
    }

    /// 逻辑空闲还要求没有因内部容量不足而保留的待刷新 staging。
    pub(super) fn is_idle(&self) -> bool {
        self.workers_idle()
            && self
                .blocked_flush_nodes
                .lock()
                .expect("blocked flush lock poisoned")
                .is_empty()
    }

    /// 任何进展都要通知:取到输出、节点关闭、出错、任务入队/完成。
    /// 否则阻塞中的宿主线程会白等到超时。
    pub(super) fn notify_activity(&self) {
        let (m, cv) = &self.activity;
        let mut a = m.lock().unwrap_or_else(|e| e.into_inner());
        a.gen = a.gen.wrapping_add(1);
        // 代数**必须**递增(防丢唤醒的本体);但没人在等时就别去做那次 futex 唤醒。
        let wake = a.waiters > 0;
        drop(a);
        if wake {
            cv.notify_all();
        }
    }

    /// 读取当前活动代数。**必须在判断 is_idle/is_done 之前读取**,再据此 `wait_activity_since`,
    /// 否则会丢唤醒:若在「判断非空闲」与「开始等待」之间任务恰好全部完成,
    /// 等待会一直睡到超时(那 55ms 的假慢就是这么来的)。
    pub(super) fn activity_gen(&self) -> u64 {
        self.activity
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .gen
    }

    /// 等到活动代数不等于 `before`(即有新进展)或超时。
    pub(super) fn wait_activity_since(&self, before: u64, timeout: std::time::Duration) {
        let (m, cv) = &self.activity;
        let mut a = m.lock().unwrap_or_else(|e| e.into_inner());
        if a.gen != before {
            return; // 已有进展,不必等(也就不必登记为等待者)
        }
        // 在**持锁**期间登记:notifier 要么看见它(会 wake),要么还没拿到锁
        // (那它递增的 gen 会让下面的谓词立刻为假)。见 `Activity` 的说明。
        a.waiters += 1;
        let (mut guard, _res) = cv
            .wait_timeout_while(a, timeout, |x| x.gen == before)
            .unwrap_or_else(|e| e.into_inner());
        guard.waiters -= 1;
        drop(guard);
    }

    /// 认领一次调用:在锁下**原子地**取一个 context 槽、按对齐时间戳弹入输入、分配序号,
    /// 放进 `ready` 待执行。成功返回 true —— 调用方应随即 `dispatch_task` 派一个任务。
    ///
    /// readiness 与弹包必须在同一把锁下完成,否则两个并发认领会取到同一时间戳的包。
    pub(super) fn try_claim(&self, n: NodeId) -> bool {
        if self.shared.has_error()
            || self.shared.is_cancelled()
            || self.paused.load(Ordering::SeqCst)
        {
            return false;
        }
        let node = &self.nodes[n];
        let mut s = node.sched.lock().expect("scheduler lock poisoned");
        if !s.opened || s.close_started {
            return false;
        }
        if s.blocked_flush.is_some() {
            return false; // 先把已完成调用的 staging 刷出去,再认领新输入
        }
        if s.in_flight >= node.max_in_flight {
            return false; // 达容量上限;释放槽后由 finish→schedule_node 重扫
        }
        // 就绪判定(会短暂锁 input_queues / input_bounds,锁序 sched→queue/bound 一致)
        let Some(ready) = node.readiness() else {
            return false;
        };
        let ts = ready.ts;
        let slot = s
            .free_slots
            .pop()
            .expect("a free slot must exist when in_flight < max");
        let seq = s.next_seq;
        s.next_seq += 1;
        s.in_flight += 1;
        s.ready.push_back((slot, seq));
        self.in_flight.fetch_add(1, Ordering::SeqCst);

        // 仍持 sched 锁弹入输入 —— 保证 readiness+pop 原子。该槽此刻独占(刚从 free 取出)。
        let ctx = unsafe { node.ctx_slot(slot) };
        ctx.reset();
        // 批处理:按就绪期算好的计划,给每个正向口整批弹包。各口取数可以不同
        // (对齐语义,见 `batch_readiness`)。input_ts = 本批末尾的对齐时间戳,下游单调。
        if let Some(plan) = &ready.batch {
            for &(port, count) in &plan.take {
                let remaining = {
                    let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                    for _ in 0..count {
                        let Some(p) = q.pop_front() else { break };
                        node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                        self.shared.on_dequeue(p.byte_size());
                        node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                        ctx.input_batches[port].push(p);
                    }
                    q.len() // 顺手读,省一次同一把锁的再获取(ADR #36)
                };
                // 每个参与口都推进到末尾时间戳之后 —— 与 sync 一致:即便某口本批一个包
                // 都没取到,也要告诉下游「本口不会再有 <= last_ts 的数据」。对齐保证了
                // 各口 <= last_ts 的包都已被消费(每轮取的是全局最小)。
                node.advance_bound(port, plan.last_ts.next_allowed_in_stream());
                ctx.inputs_done[port] =
                    node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
            }
            // 反馈口:多输入口的 batch 才使 batch + back_edges 成为可能(单口时它凑不出
            // 「至少一个正向口 + 一个反馈口」)。语义与其它策略一致 —— 每次触发读一次最新值,
            // 不参与对齐、不推进 bound。
            for port in 0..node.input_queues.len() {
                if !node.input_is_back_edge[port] {
                    continue;
                }
                let (popped, remaining) = {
                    let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                    let p = q.pop_front();
                    (p, q.len())
                };
                if let Some(p) = popped {
                    node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                    self.shared.on_dequeue(p.byte_size());
                    node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                    ctx.inputs[port] = Some(p);
                }
                ctx.inputs_done[port] =
                    node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
            }
            ctx.input_ts = plan.last_ts;
            return true;
        }
        for port in 0..node.input_queues.len() {
            if node.input_is_back_edge[port] {
                // 反馈寄存器:取最新一包(队列 cap-1),不参与 ts 对齐、不推进 bound。
                // 首拍(尚无反馈)队列为空 → ctx.inputs[port] = None,内核看到空反馈,自处理。
                let (popped, remaining) = {
                    let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                    let p = q.pop_front();
                    (p, q.len())
                };
                if let Some(p) = popped {
                    node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                    self.shared.on_dequeue(p.byte_size());
                    node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                    ctx.inputs[port] = Some(p);
                }
                ctx.inputs_done[port] =
                    node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
                continue;
            }
            // 只处理「参与本次触发」的口(SyncSet:就绪组;其余策略:全部口)。
            // 非参与口原样不动:不弹包、不推进 bound —— 它的包(可能属别的组)留给下次。
            let participates = ready.ports.as_ref().is_none_or(|set| set.contains(&port));
            if !participates {
                continue;
            }
            // 只取时间戳恰好等于 ts 的包;某口在该时刻没有数据是合法的(算子看到空包),
            // 这正是时间戳对齐的语义 —— 若无条件每口弹一个,就会把不同时刻的数据配到一起。
            // 一次临界区办三件事:读队首 ts、按需弹包、读剩余长度。
            // 原先 `front_ts` / `pop_front` / `queue_len` 各拿一次**同一把**队列锁(每口 3 次)。
            // 安全性:全程持 `sched`,而只有 `try_claim` 会 pop(ADR #30 pop-at-claim),
            // 别的线程只 push(追加尾部、不动队首)—— 故队首稳定。
            // 只取时间戳恰好等于 ts 的包;某口在该时刻没有数据是合法的(算子看到空包),
            // 这正是时间戳对齐的语义 —— 若无条件每口弹一个,就会把不同时刻的数据配到一起。
            let (popped, remaining) = {
                let mut q = node.input_queues[port].lock().expect("queue lock poisoned");
                let hit = q.front().map(|p| p.timestamp()) == Some(ts);
                let p = if hit { q.pop_front() } else { None };
                (p, q.len())
            };
            if let Some(p) = popped {
                node.input_queue_bytes[port].fetch_sub(p.byte_size(), Ordering::SeqCst);
                self.shared.on_dequeue(p.byte_size());
                node.stats.packets_in.fetch_add(1, Ordering::Relaxed);
                ctx.inputs[port] = Some(p);
            }
            node.advance_bound(port, ts.next_allowed_in_stream());
            // `remaining` 与 pop 同一临界区内读得。这不改语义:`inputs_done` 还要求
            // `input_closed`,而关流后不再有 push,长度已稳定。
            ctx.inputs_done[port] =
                node.input_closed[port].load(Ordering::SeqCst) && remaining == 0;
        }
        // 源节点无输入包,用认领序号当单调时间戳(auto-emit 继承 → 下游单调,复用 seq 重排)。
        ctx.input_ts = if node.is_source() {
            Timestamp(seq as i64)
        } else {
            ts
        };
        true
    }

    /// 尽力填满容量:反复认领并派任务,直到无法再认领。
    /// `max_in_flight == 1` 时每轮至多派一个,与串行行为一致。
    pub(super) fn schedule_node(&self, n: NodeId) {
        while self.try_claim(n) {
            self.dispatch_task(n);
            // 本次认领已从某些内部输入队列弹包,可能为上游腾出了容量。
            self.resume_blocked_flushes();
        }
    }

    /// 跑一个委托给宿主线程的任务。返回是否真的跑了。
    ///
    /// ⚠ 必须先把 pop 的结果落到局部变量:在 edition 2021 里,`if let` 表达式中的
    /// 临时值(此处是 MutexGuard)会存活到整个 if-let 块结束 —— 那样 `run_node`
    /// 内部再去锁同一个委托队列就自锁死。这也是 R2 锁序规则的实例。
    pub(super) fn run_one_main_task(&self) -> bool {
        let next = self
            .executors
            .iter()
            .find_map(|executor| executor.take_delegated());
        match next {
            Some(n) => {
                self.run_node(n);
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.notify_activity();
                true
            }
            None => false,
        }
    }

    /// 执行一步:跑一个主线程任务,或推进关流。返回是否真的做了事。
    pub(super) fn pump_step(&self) -> bool {
        self.run_one_main_task() || self.try_advance_closing()
    }

    pub(super) fn run_node(&self, n: NodeId) {
        let node = &self.nodes[n];
        // 取出一个待执行的调用(认领时已把输入弹进对应槽)。
        let inv = {
            node.sched
                .lock()
                .expect("scheduler lock poisoned")
                .ready
                .pop_front()
        };
        let Some((slot, seq)) = inv else {
            // 认领与派任务 1:1,理论上不会为空;稳妥起见走一遍收尾。
            self.finish(n);
            return;
        };

        // 契约类型校验(在本槽上)。类型不符宁可报错,也不让算子按错误类型解读内存。
        let ok = match self.check_input_types(n, slot) {
            Err(e) => self.on_node_error(n, slot, e),
            Ok(()) => {
                let rc = self.call_kernel(n, slot, KernelPhase::Process);
                // 源节点:内核调了 source_done() → 记下,readiness 不再放行、随后关流终止。
                if node.is_source() && unsafe { node.ctx_slot(slot) }.source_done {
                    node.source_done.store(true, Ordering::SeqCst);
                }
                if rc != 0 {
                    let e = unsafe { node.ctx_slot(slot) }.take_error(rc);
                    self.on_node_error(n, slot, e)
                } else if let Err(e) = self.check_output_types(n, slot) {
                    self.on_node_error(n, slot, e)
                } else {
                    node.stats.processed.fetch_add(1, Ordering::Relaxed);
                    true
                }
            }
        };
        self.complete_invocation(n, slot, seq, ok);
    }

    /// 调用完成:按 `seq` 顺序刷新输出并释放槽。保证下游看到的时间戳单调 ——
    /// 即使后面的时间戳先算完,也要等前面的先刷。
    /// 算子失败时按本节点的 [`OnError`] 分流。返回值是给 `complete_invocation` 的 `ok`:
    /// **`true` 表示「走刷新路径」**(而非「成功」)。
    ///
    /// `Skip` 之所以返回 `true`,是因为刷新路径才会**推进下游时间戳边界** ——
    /// staging 已被 `discard_staging` 清空,于是 `flush_one` 落到「无产出」分支,
    /// 自动 `propagate_bound(input_ts + 1)`。这正是 `Filter` 丢包时依赖的同一套机制。
    /// 不推进边界的话下游会永远等这一刻,等于把一帧出错升级成整图卡死。
    pub(super) fn on_node_error(&self, n: NodeId, slot: usize, e: Error) -> bool {
        let node = &self.nodes[n];
        let before = node.stats.errors.fetch_add(1, Ordering::Relaxed);
        match node.on_error {
            OnError::Abort => {
                self.shared.record_error(e);
                false // 丢弃产出,不刷新;has_error 置位后调度不再放行 → 全图终止
            }
            OnError::Skip => {
                // 有损行为绝不静默:计数(node_stats().errors)+ 打 WARN。
                // 指数退避,避免每帧都错时刷爆日志(与 note_dropped 同法)。
                let after = before + 1;
                if before == 0 || after.is_power_of_two() {
                    runtime::log_warn(&format!(
                        "node `{}`: skipping a failed packet (on_error=skip), {} so far: {}",
                        node.name, after, e
                    ));
                }
                // 清掉这一包可能已写了一半的产出,再走刷新路径(仅为推进边界)。
                unsafe { node.ctx_slot(slot) }.discard_staging();
                true
            }
        }
    }

    pub(super) fn complete_invocation(&self, n: NodeId, slot: usize, seq: u64, ok: bool) {
        let node = &self.nodes[n];
        // 登记结果;当前无人刷新则由本线程担任刷新者。
        //
        // **快路**:重排缓冲为空、本次恰好就是待刷新序号、且当前无人刷新 —— 直接接手,
        // 不必经 `pending_flush`。`max_in_flight == 1`(默认)时这条恒成立:同一时刻只有
        // 一次调用在飞,`seq` 必然等于 `next_flush_seq`。避免每次调用一次 BTreeMap
        // 插入 + 删除(perf 实测这对增删连带堆分配约占 5~6%)。
        let mut first: Option<(usize, bool)> = None;
        let be_flusher = {
            let mut s = node.sched.lock().expect("scheduler lock poisoned");
            if !s.flushing
                && s.blocked_flush.is_none()
                && s.pending_flush.is_empty()
                && seq == s.next_flush_seq
            {
                s.flushing = true;
                first = Some((slot, ok));
                true
            } else {
                s.pending_flush.insert(seq, (slot, ok));
                if s.flushing {
                    false
                } else {
                    s.flushing = true;
                    true
                }
            }
        };
        if be_flusher {
            self.drive_invocation_flushes(n, first);
        }
        self.finish(n);
    }

    /// 契约声明的输入类型校验。类型不符宁可报错,也不让算子按错误类型解读内存。
    pub(super) fn check_input_types(&self, n: NodeId, slot: usize) -> Result<()> {
        let node = &self.nodes[n];
        let ctx = unsafe { node.ctx_slot(slot) };
        for (port, &want) in node.input_types.iter().enumerate() {
            let Some(pkt) = ctx.inputs.get(port).and_then(|s| s.as_ref()) else {
                continue;
            };
            if pkt.is_empty() {
                continue; // 空包(时间戳边界)不参与类型校验
            }
            let got = pkt.type_id();

            // `HOST_OBJECT` 预留未启用(ADR #26)。契约声明它已在建图期拒掉,但包**自己**
            // 带 7 是另一条路(C 侧手填 type_id,或 Rust unsafe `from_foreign`)。这一条必须在
            // `want == 0` 的短路**之前**判 —— 否则声明 `any` 的端口(最常见的情形)恰好
            // 就是漏网的那种,而那正是要堵的洞。
            if got == crate::packet::type_id::HOST_OBJECT {
                return Err(Error::Kernel(format!(
                    "[{}] input port `{}` carries LMFLOW_TYPE_HOST_OBJECT, which is reserved \
                     and not enabled (see ADR #26); use LMFLOW_TYPE_BUFFER for numeric \
                     collections, or LMFLOW_TYPE_STR carrying JSON for arbitrary metadata",
                    node.name,
                    node.in_ports.name(port).unwrap_or("?"),
                )));
            }

            if want == 0 {
                continue; // 未声明类型 = 接受任意
            }
            if got != want {
                // `got == NONE` 是一个**有明确出路**的特例,但出路取决于包是谁造的 ——
                // 按 payload 形态分别给建议,否则会把 Rust API 推给 C/C++ 宿主(或反之)。
                // NONE 的来源不止一个:`Packet::new`(Native)、`from_foreign(.., 0, ..)`、
                // 以及 C ABI 侧 type_id 填 0 的自建包(Foreign)。
                let hint = if got == crate::packet::type_id::NONE {
                    match pkt.payload() {
                        Some(crate::packet::Payload::Foreign(_)) => {
                            " (the packet carries no declared type: its type_id is \
                             LMFLOW_TYPE_NONE, which means \"skip type checking\"; set a real \
                             LMFLOW_TYPE_* on the packet you submit, or declare this port as \
                             any-type)"
                        }
                        // Native = Rust 原生 payload,只可能来自 Rust 宿主
                        _ => {
                            " (the packet carries no declared type because its payload is \
                             Rust-native, e.g. built with `Packet::new`; use \
                             `Packet::from_i64` / `from_f64` / `from_builtin` for built-in \
                             payloads, implement `InteropType` and use `Packet::from_interop` \
                             for a custom type, or use unsafe `Packet::new_interop` only after \
                             manually proving the ABI layout)"
                        }
                    }
                } else {
                    ""
                };
                return Err(Error::Kernel(format!(
                    "[{}] input port `{}` type mismatch: contract declares {}, actual {}{}",
                    node.name,
                    node.in_ports.name(port).unwrap_or("?"),
                    crate::packet::type_name(want),
                    crate::packet::type_name(got),
                    hint,
                )));
            }
        }
        Ok(())
    }

    /// 算子暂存输出的类型校验。必须在离开回调后、派发前统一做,因为 C/C++/Python 的
    /// `emit` ABI 是 `void`:不能依赖算子检查返回值。放在这里也能覆盖源节点、图输出、
    /// `close` 产出以及所有语言的算子。
    pub(super) fn check_output_types(&self, n: NodeId, slot: usize) -> Result<()> {
        let node = &self.nodes[n];
        let ctx = unsafe { node.ctx_slot(slot) };
        for (port, packets) in ctx.staging.iter().enumerate() {
            let want = node.output_types[port];
            for pkt in packets {
                if pkt.is_empty() {
                    continue;
                }
                let got = pkt.type_id();
                if got == crate::packet::type_id::HOST_OBJECT {
                    return Err(Error::Kernel(format!(
                        "[{}] output port `{}` carries LMFLOW_TYPE_HOST_OBJECT, which is reserved \
                         and not enabled (see ADR #26); use LMFLOW_TYPE_BUFFER for numeric \
                         collections, or LMFLOW_TYPE_STR carrying JSON for arbitrary metadata",
                        node.name,
                        node.out_ports.name(port).unwrap_or("?"),
                    )));
                }
                if want != crate::packet::type_id::NONE && got != want {
                    return Err(Error::Kernel(format!(
                        "[{}] output port `{}` type mismatch: contract declares {}, actual {}",
                        node.name,
                        node.out_ports.name(port).unwrap_or("?"),
                        crate::packet::type_name(want),
                        crate::packet::type_name(got),
                    )));
                }
            }
        }
        Ok(())
    }

    /// 把时间戳边界推给某条边的所有消费者,并重扫其就绪性。
    pub(super) fn propagate_bound(&self, edge: EdgeId, bound: Timestamp) {
        let consumers: Vec<(NodeId, usize)> = self.edges[edge].consumers.clone();
        for (node, port) in consumers {
            self.nodes[node].advance_bound(port, bound);
            self.schedule_node(node);
        }
    }

    /// 调用算子回调(在指定 context 槽上)。**调用期间不持有任何引擎锁**(R1),
    /// 并记录耗时以便定位卡死。可被并发调用(不同槽),故 `process` 必须可重入。
    pub(super) fn call_kernel(&self, n: NodeId, slot: usize, phase: KernelPhase) -> i32 {
        let node = &self.nodes[n];
        // 直接交出 UnsafeCell 内部指针:不构造 Rust 引用,故与回调内
        // 从该指针造出的 `&mut Context` 不冲突(该槽此刻由本调用独占持有)。
        let ctx_ptr = node.ctxs[slot].get() as *mut c_void;
        // 记账全走原子:改造前这里每次调用要拿 2 次 running_timing 锁 + 1 次 stats 锁
        // (再加 run_node 里的 processed 一次)。R1 要求「调算子时不持任何引擎锁」——
        // 原子天然满足,也顺带把 4 对 mutex 从每包热路径上去掉了。
        // 计时可关(见 `GraphConfig::stats_timing`):`Instant::now()` + 末尾的 `elapsed()`
        // 是**每次 process 两次**时钟读,本机约 43 ns、占单跳派发成本约 15%。
        // 一次时钟读两用:既作本次耗时起点,也作「本节点开始在跑」的时刻。
        let started = if self.timing {
            Some(Instant::now())
        } else {
            None
        };
        if node.stats.in_flight.fetch_add(1, Ordering::Relaxed) == 0 {
            if let Some(t) = started {
                let since_epoch = t.saturating_duration_since(self.epoch).as_micros() as i64;
                node.stats.started_us.store(since_epoch, Ordering::Relaxed);
            }
        }

        // 源节点定速(见 `NodeConfig::rate`):在**调算子之前**、于本节点的池线程里
        // sleep 到点,保证相邻两次 process 至少隔 min_period。R1 未被破坏 —— 此刻不持任何
        // 引擎锁(last_fire 那把在 sleep 前已释放)。只有源节点会设 min_period;它本就串行
        // 自续产(finish→schedule_node→try_claim 紧循环),故这里节流就等于给整条产出限速。
        if matches!(phase, KernelPhase::Process) {
            if let Some(period) = node.min_period {
                let now = Instant::now();
                let wait = {
                    let mut last = node.last_fire.lock().expect("last_fire lock poisoned");
                    let w = match *last {
                        Some(prev) => period.checked_sub(now.duration_since(prev)),
                        None => None, // 首次不等
                    };
                    // 预支下一次的基准:按「本次实际放行时刻」记,避免累积漂移。
                    *last = Some(now + w.unwrap_or_default());
                    w
                }; // 锁在此释放,再 sleep —— 不持锁阻塞
                if let Some(w) = wait {
                    std::thread::sleep(w);
                }
            }
        }

        // 安全性:ctx_ptr 来自本槽的 UnsafeCell,该槽此刻独占。
        let rc = unsafe {
            match phase {
                KernelPhase::Open => node.kernel.open(ctx_ptr),
                KernelPhase::Process => node.kernel.process(ctx_ptr),
                KernelPhase::Close => node.kernel.close(ctx_ptr),
            }
        };

        // 归零时**不清** started_us:读侧按 in_flight > 0 判断是否在跑,
        // 故无需清零,也就不存在「清零」与「新一次开始」互相覆盖的竞争。
        node.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
        let Some(t0) = started else { return rc }; // 计时关闭:统计与 watchdog 都不适用
        let us = t0.elapsed().as_micros() as i64;
        if matches!(phase, KernelPhase::Process) {
            node.stats.total_us.fetch_add(us, Ordering::Relaxed);
            node.stats.max_us.fetch_max(us, Ordering::Relaxed);
        }
        let wd = self.shared.config.watchdog_ms;
        if wd > 0 && us as u64 > wd * 1000 {
            runtime::log_warn(&format!(
                "node `{}`: one {:?} took {} ms, exceeding watchdog {} ms",
                node.name,
                phase,
                us / 1000,
                wd
            ));
        }
        rc
    }

    pub fn add_observer(
        &self,
        port: &str,
        cb: unsafe extern "C" fn(*mut c_void, crate::ffi::LMFlowPacket),
        user: *mut c_void,
    ) -> Result<()> {
        let edge = *self
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("graph output port `{port}` does not exist")))?;
        self.edges[edge]
            .observers
            .lock()
            .expect("observer list lock poisoned")
            .push(Observer::C { cb, user });
        Ok(())
    }

    /// Rust 宿主的推模式订阅。回调在**派发该包的线程**上执行(可能是池线程),
    /// 因此必须 `Send + Sync`;回调内不得再调 graph 的生命周期接口。
    pub fn add_observer_fn(&self, port: &str, f: Arc<dyn Fn(&Packet) + Send + Sync>) -> Result<()> {
        let edge = *self
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("graph output port `{port}` does not exist")))?;
        self.edges[edge]
            .observers
            .lock()
            .expect("observer list lock poisoned")
            .push(Observer::Rust(f));
        Ok(())
    }

    // ffi 层需要的只读访问
    pub fn nodes_len(&self) -> usize {
        self.nodes.len()
    }
    pub fn node_name_at(&self, i: usize) -> Option<&str> {
        self.nodes.get(i).map(|n| n.name.as_str())
    }
    pub fn node_input_ports_len(&self, node: usize) -> usize {
        self.nodes.get(node).map_or(0, |value| value.in_ports.len())
    }
    pub fn node_input_port_name_at(&self, node: usize, port: usize) -> Option<&str> {
        self.nodes.get(node)?.in_ports.name(port)
    }
    pub fn input_port_name_at(&self, i: usize) -> Option<&str> {
        self.graph_inputs
            .get(i)
            .map(|&e| self.edges[e].name.as_str())
    }
    pub fn output_port_name_at(&self, i: usize) -> Option<&str> {
        self.graph_outputs
            .get(i)
            .map(|&e| self.edges[e].name.as_str())
    }
    pub fn num_input_ports(&self) -> usize {
        self.graph_inputs.len()
    }
    pub fn num_output_ports(&self) -> usize {
        self.graph_outputs.len()
    }
    pub fn edge_id_by_name(&self, name: &str) -> Option<EdgeId> {
        self.edge_by_name.get(name).copied()
    }
    pub fn input_edge_by_name(&self, name: &str) -> Option<EdgeId> {
        self.input_by_name.get(name).copied()
    }
    pub fn queue_depth_by_name(&self, name: &str) -> Option<usize> {
        Some(self.queue_depth(self.edge_id_by_name(name)?))
    }
    pub fn dropped_by_name(&self, name: &str) -> Option<u64> {
        Some(self.edges[self.edge_id_by_name(name)?].dropped_count())
    }
    pub fn send_by_edge(&self, edge: EdgeId, pkt: Packet, blocking: bool) -> Result<()> {
        self.send(edge, pkt, blocking)
    }
    pub(crate) fn watermark_backpressure_stats(
        &self,
        edge: EdgeId,
    ) -> WatermarkBackpressureStatsSnapshot {
        let edge = &self.edges[edge];
        let stats = edge.watermark_backpressure.snapshot(self.epoch_us());
        WatermarkBackpressureStatsSnapshot {
            port_name: edge.name.clone(),
            packet_limit: self.shared.config.max_queued_packets,
            total_queued_packets: self.shared.total_queued(),
            blocked: stats.blocked,
            active_waiters: stats.active_waiters,
            blocked_for_us: stats.blocked_for_us,
            block_events: stats.block_events,
            total_blocked_us: stats.total_blocked_us,
        }
    }
    pub fn close_edge_pub(&self, edge: EdgeId) {
        self.close_edge(edge);
        self.set_state_draining_if_all_inputs_closed();
    }
    pub fn state_pub(&self) -> State {
        self.state()
    }
    pub fn dump_pub(&self) -> String {
        self.dump()
    }
    pub fn node_stats_pub(&self, i: usize) -> Option<NodeStatsSnapshot> {
        self.node_stats(i)
    }
    pub fn side_packets_mut(&self) -> &Mutex<BTreeMap<String, Packet>> {
        &self.side_packets
    }
    pub fn start_pub(self: &Arc<Self>) -> Result<()> {
        self.start()
    }
    pub fn wait_done_pub(&self) -> Result<()> {
        self.wait_done(None)
    }
    pub fn pump_step_pub(&self) -> bool {
        self.pump_step()
    }
    pub fn all_nodes_closed_pub(&self) -> bool {
        self.all_nodes_closed()
    }
    pub fn graph_inputs(&self) -> &[EdgeId] {
        &self.graph_inputs
    }

    // ---- Poller 需要的内部能力 ----
    pub(crate) fn remaining_for_poller(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> Option<std::time::Duration> {
        self.remaining(deadline)
    }
    pub(crate) fn wait_activity_since_pub(&self, before: u64, d: std::time::Duration) {
        self.wait_activity_since(before, d);
    }
    pub(crate) fn activity_gen_pub(&self) -> u64 {
        self.activity_gen()
    }
    pub(crate) fn is_idle_pub(&self) -> bool {
        self.is_idle()
    }

    // ---- 暂停 / 恢复 ----
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// 恢复调度,并重扫一遍就绪节点 —— 暂停期间到达的包否则会一直躺着。
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        for n in 0..self.nodes.len() {
            self.schedule_node(n);
        }
        self.notify_activity();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn executor_names(&self) -> Vec<&str> {
        self.executors.iter().map(|p| p.name()).collect()
    }

    pub(crate) fn shutdown_executors_pub(&self) {
        self.shutdown_executors();
    }

    /// 关停所有线程池并 join。委托执行器是 no-op。**必须在动节点之前做**。
    pub(super) fn shutdown_executors(&self) {
        for p in &self.executors {
            p.shutdown();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum KernelPhase {
    Open,
    Process,
    Close,
}
