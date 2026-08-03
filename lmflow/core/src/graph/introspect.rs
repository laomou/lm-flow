//! 内省:队列深度、节点统计快照、人读 dump。
//!
//! 与 [`super::dot`] 同理 —— 纯「读快照 + 格式化」,不参与调度、不碰锁序,
//! 单独成模块让 `mod.rs` 只剩并发核心。本模块是 `graph` 的子模块,故仍可访问私有字段。

use std::sync::atomic::Ordering;

use super::{EdgeId, GraphInner, NodeStatsSnapshot};

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
        let running_for_us = if running {
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
