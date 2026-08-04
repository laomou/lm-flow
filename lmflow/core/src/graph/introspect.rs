//! 内省:队列深度、节点统计快照、人读 dump。
//!
//! 与 [`super::dot`] 同理 —— 纯「读快照 + 格式化」,不参与调度、不碰锁序,
//! 单独成模块让 `mod.rs` 只剩并发核心。本模块是 `graph` 的子模块,故仍可访问私有字段。

use std::sync::atomic::Ordering;

use super::{EdgeId, GraphInner, InputQueueStatsSnapshot, NodeStatsSnapshot};

impl GraphInner {
    pub(super) fn queue_depth(&self, edge: EdgeId) -> usize {
        self.edges[edge]
            .consumers
            .iter()
            .map(|&(n, p)| self.nodes[n].queue_len(p))
            .sum()
    }

    pub(super) fn node_stats(&self, i: usize) -> Option<NodeStatsSnapshot> {
        let node = self.nodes.get(i)?;
        let st = &node.stats;
        // 先看 in_flight 再用 started_us —— 后者归零时不清,只在「在跑」时有意义。
        let running = st.in_flight.load(Ordering::Relaxed) > 0;
        // 计时关闭时 started_us 从未写过 —— 必须报 0,不能拿它去算(见 GraphConfig::stats_timing)。
        let running_for_us = if running && self.timing {
            let now_us = self.epoch.elapsed().as_micros() as i64;
            (now_us - st.started_us.load(Ordering::Relaxed)).max(0)
        } else {
            0
        };
        Some(NodeStatsSnapshot {
            node_name: node.name.clone(),
            kernel_name: node.kernel_name.clone(),
            running,
            running_for_us,
            processed: st.processed.load(Ordering::Relaxed),
            errors: st.errors.load(Ordering::Relaxed),
            total_process_us: st.total_us.load(Ordering::Relaxed),
            max_process_us: st.max_us.load(Ordering::Relaxed),
            packets_in: st.packets_in.load(Ordering::Relaxed),
            packets_out: st.packets_out.load(Ordering::Relaxed),
            peak_queue_depth: st.peak_queue_depth.load(Ordering::Relaxed),
            queued: (0..node.input_queues.len())
                .map(|p| node.queue_len(p))
                .sum(),
        })
    }

    pub(super) fn input_queue_stats(
        &self,
        node_id: usize,
        port: usize,
    ) -> Option<InputQueueStatsSnapshot> {
        let node = self.nodes.get(node_id)?;
        let port_name = node.in_ports.name(port)?.to_string();
        let stats = node.input_queue_stats.get(port)?;
        let queued_packets = node.queue_len(port);
        let queued_bytes = node.input_queue_bytes[port].load(Ordering::SeqCst);
        let reserved_packets = node.input_queue_reserved[port].load(Ordering::SeqCst);
        let since = stats.blocked_since_us.load(Ordering::SeqCst);
        let blocked_for_us = if since == 0 {
            0
        } else {
            let now = self.epoch.elapsed().as_micros().min(i64::MAX as u128) as i64;
            now.saturating_sub(since.saturating_sub(1)).max(0) as u64
        };
        let edge = node.inputs[port];
        let producer_name = self.edges[edge]
            .producer
            .map(|producer| self.nodes[producer].name.clone());
        Some(InputQueueStatsSnapshot {
            node_name: node.name.clone(),
            port_name,
            producer_name,
            packet_capacity: node.input_queue_capacity[port],
            queued_packets,
            queued_bytes,
            reserved_packets,
            peak_queued_packets: stats.peak_packets.load(Ordering::Relaxed),
            peak_queued_bytes: stats.peak_bytes.load(Ordering::Relaxed),
            blocked: since != 0,
            blocked_for_us,
            block_events: stats.block_events.load(Ordering::Relaxed),
            total_blocked_us: stats
                .blocked_total_us
                .load(Ordering::Relaxed)
                .saturating_add(blocked_for_us),
        })
    }

    pub(super) fn dump(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "graph state={:?} nodes={} edges={} queued={} ({} bytes)\n",
            self.state(),
            self.nodes.len(),
            self.edges.len(),
            self.shared.total_queued(),
            self.shared.total_queued_bytes()
        ));
        s.push_str("node              state        queued  processed  errors  avg_us  max_us\n");
        for i in 0..self.nodes.len() {
            let st = self.node_stats(i).expect("node exists");
            let sched = self.nodes[i].sched.lock().expect("scheduler lock poisoned");
            let state = if st.running {
                format!("RUNNING {}ms", st.running_for_us / 1000)
            } else if sched.closed {
                "closed".to_string()
            } else if sched.opened {
                "idle".to_string()
            } else {
                "new".to_string()
            };
            let avg = if st.processed > 0 {
                st.total_process_us / st.processed as i64
            } else {
                0
            };
            s.push_str(&format!(
                "{:<17} {:<12} {:>6}  {:>9}  {:>6}  {:>6}  {:>6}\n",
                st.node_name, state, st.queued, st.processed, st.errors, avg, st.max_process_us
            ));
            for port in 0..self.nodes[i].input_queues.len() {
                let queue = self.input_queue_stats(i, port).expect("input port exists");
                let capacity = queue
                    .packet_capacity
                    .map_or_else(|| "unbounded".to_string(), |value| value.to_string());
                s.push_str(&format!(
                    "  input `{}` capacity={} packets queued={}/{}B reserved={} peak={}/{}B blocked={} events={} total={}us\n",
                    queue.port_name,
                    capacity,
                    queue.queued_packets,
                    queue.queued_bytes,
                    queue.reserved_packets,
                    queue.peak_queued_packets,
                    queue.peak_queued_bytes,
                    queue.blocked,
                    queue.block_events,
                    queue.total_blocked_us,
                ));
            }
        }
        for e in &self.edges {
            s.push_str(&format!(
                "edge `{}` closed={} dropped={}\n",
                e.name,
                e.is_closed(),
                e.dropped_count()
            ));
        }
        s
    }
}
