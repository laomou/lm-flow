//! Host-facing and FFI-facing `GraphInner` facade.

use super::*;

impl GraphInner {
    pub fn add_observer(
        &self,
        port: &str,
        cb: unsafe extern "C" fn(*mut c_void, crate::ffi::LMFlowPacket),
        user: *mut c_void,
        observe_timestamp_bounds: bool,
    ) -> Result<()> {
        let edge = *self
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("graph output port `{port}` does not exist")))?;
        if observe_timestamp_bounds {
            self.edges[edge]
                .has_timestamp_bound_subscriber
                .store(true, Ordering::Relaxed);
        }
        self.edges[edge]
            .observers
            .lock()
            .expect("observer list lock poisoned")
            .push(Observer::C {
                cb,
                user,
                observe_timestamp_bounds,
            });
        Ok(())
    }

    /// Rust 宿主的推模式订阅。回调在**派发该包的线程**上执行(可能是池线程),
    /// 因此必须 `Send + Sync`;回调内不得再调 graph 的生命周期接口。
    pub fn add_observer_fn(
        &self,
        port: &str,
        callback: Arc<dyn Fn(&Packet) + Send + Sync>,
        observe_timestamp_bounds: bool,
    ) -> Result<()> {
        let edge = *self
            .output_by_name
            .get(port)
            .ok_or_else(|| Error::NotFound(format!("graph output port `{port}` does not exist")))?;
        if observe_timestamp_bounds {
            self.edges[edge]
                .has_timestamp_bound_subscriber
                .store(true, Ordering::Relaxed);
        }
        self.edges[edge]
            .observers
            .lock()
            .expect("observer list lock poisoned")
            .push(Observer::Rust {
                callback,
                observe_timestamp_bounds,
            });
        Ok(())
    }

    pub fn nodes_len(&self) -> usize {
        self.nodes.len()
    }

    pub fn node_name_at(&self, i: usize) -> Option<&str> {
        self.nodes.get(i).map(|node| node.name.as_str())
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
            .map(|&edge| self.edges[edge].name.as_str())
    }

    pub fn output_port_name_at(&self, i: usize) -> Option<&str> {
        self.graph_outputs
            .get(i)
            .map(|&edge| self.edges[edge].name.as_str())
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

    pub fn send_by_edge(&self, edge: EdgeId, packet: Packet, blocking: bool) -> Result<()> {
        self.send(edge, packet, blocking)
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
        let generation = self.wakeup_generation.load(Ordering::Acquire);
        let progressed = self.pump_step();
        if !progressed {
            self.wakeup_pending.store(false, Ordering::Release);
            if self.wakeup_generation.load(Ordering::Acquire) != generation
                || self.delegated_tasks_pending()
            {
                self.request_wakeup();
            }
        }
        progressed
    }

    pub fn all_nodes_closed_pub(&self) -> bool {
        self.all_nodes_closed()
    }

    pub fn graph_inputs(&self) -> &[EdgeId] {
        &self.graph_inputs
    }

    pub(crate) fn remaining_for_poller(
        &self,
        deadline: Option<std::time::Instant>,
    ) -> Option<std::time::Duration> {
        self.remaining(deadline)
    }

    pub(crate) fn wait_activity_since_pub(&self, before: u64, duration: std::time::Duration) {
        self.wait_activity_since(before, duration);
    }

    pub(crate) fn activity_gen_pub(&self) -> u64 {
        self.activity_gen()
    }

    pub(crate) fn is_idle_pub(&self) -> bool {
        self.is_idle()
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// 恢复调度,并重扫一遍就绪节点 —— 暂停期间到达的包否则会一直躺着。
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        for node in 0..self.nodes.len() {
            self.schedule_node(node);
        }
        self.notify_activity();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn executor_names(&self) -> Vec<&str> {
        self.executors
            .iter()
            .map(|executor| executor.name())
            .collect()
    }

    pub(super) fn executor_stats(&self, name: &str) -> Option<ExecutorStatsSnapshot> {
        let executor = self
            .executors
            .iter()
            .find(|executor| executor.name() == name)?;
        let stats = executor.stats();
        let capacity = if executor.is_delegating() {
            1
        } else {
            executor.num_threads()
        };
        let mut queued_counts = std::collections::BTreeMap::<NodeId, usize>::new();
        for node in executor.queued_nodes() {
            *queued_counts.entry(node).or_default() += 1;
        }
        let mut queued_nodes = queued_counts.into_iter().collect::<Vec<_>>();
        queued_nodes.sort_by(|(left_node, left_count), (right_node, right_count)| {
            right_count.cmp(left_count).then_with(|| {
                self.nodes[*left_node]
                    .name
                    .cmp(&self.nodes[*right_node].name)
            })
        });
        Some(ExecutorStatsSnapshot {
            queued: stats.queued,
            running: stats.running,
            peak_queued: stats.peak_queued,
            completed: stats.completed,
            total_wait_us: stats.total_wait_us,
            total_execution_us: stats.total_execution_us,
            queued_for_us: stats.queued_for_us,
            saturated: stats.queued > 0 && stats.running >= capacity,
            queued_nodes: queued_nodes
                .into_iter()
                .take(5)
                .map(|(node, count)| format!("{} ({count})", self.nodes[node].name))
                .collect(),
        })
    }

    pub(crate) fn shutdown_executors_pub(&self) {
        self.shutdown_executors();
    }

    /// 关停所有线程池并 join。委托执行器是 no-op。**必须在动节点之前做**。
    pub(super) fn shutdown_executors(&self) {
        for executor in &self.executors {
            executor.shutdown();
        }
    }
}
