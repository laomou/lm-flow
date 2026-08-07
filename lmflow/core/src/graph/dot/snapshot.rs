use crate::graph::{DotView, GraphInner, LATENCY_BUCKETS};
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DotNodeCounters {
    pub(super) processed: u64,
    pub(super) errors: u64,
    pub(super) total_us: i64,
    pub(super) packets_in: u64,
    pub(super) packets_out: u64,
    pub(super) latency_buckets: [u64; LATENCY_BUCKETS],
    pub(super) cow_copies: u64,
    pub(super) cow_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DotPressureCounters {
    pub(super) block_events: u64,
    pub(super) total_blocked_us: u64,
    pub(super) dropped: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DotIntervalSnapshot {
    pub(super) at_us: i64,
    pub(super) nodes: Vec<DotNodeCounters>,
    pub(super) ports: Vec<Vec<DotPressureCounters>>,
    pub(super) edges: Vec<DotPressureCounters>,
    pub(super) pollers: Vec<Vec<DotPressureCounters>>,
}

#[derive(Debug, Default)]
pub(in crate::graph) struct DotIntervalBaselines {
    pub(super) compact: Option<DotIntervalSnapshot>,
    pub(super) diagnostics: Option<DotIntervalSnapshot>,
}

#[derive(Debug, Clone)]
pub(super) struct DotInterval {
    pub(super) elapsed_us: u64,
    pub(super) first: bool,
    pub(super) current: DotIntervalSnapshot,
    pub(super) previous: DotIntervalSnapshot,
}

impl DotInterval {
    pub(super) fn node(&self, node: usize) -> DotNodeCounters {
        let current = self.current.nodes[node];
        let previous = self.previous.nodes[node];
        DotNodeCounters {
            processed: current.processed.saturating_sub(previous.processed),
            errors: current.errors.saturating_sub(previous.errors),
            total_us: current.total_us.saturating_sub(previous.total_us),
            packets_in: current.packets_in.saturating_sub(previous.packets_in),
            packets_out: current.packets_out.saturating_sub(previous.packets_out),
            latency_buckets: std::array::from_fn(|bucket| {
                current.latency_buckets[bucket].saturating_sub(previous.latency_buckets[bucket])
            }),
            cow_copies: current.cow_copies.saturating_sub(previous.cow_copies),
            cow_bytes: current.cow_bytes.saturating_sub(previous.cow_bytes),
        }
    }

    pub(super) fn port(&self, node: usize, port: usize) -> DotPressureCounters {
        pressure_delta(
            self.current.ports[node][port],
            self.previous.ports[node][port],
        )
    }

    pub(super) fn edge(&self, edge: usize) -> DotPressureCounters {
        pressure_delta(self.current.edges[edge], self.previous.edges[edge])
    }

    pub(super) fn poller(&self, edge: usize, poller: usize) -> DotPressureCounters {
        pressure_delta(
            self.current.pollers[edge][poller],
            self.previous
                .pollers
                .get(edge)
                .and_then(|pollers| pollers.get(poller))
                .copied()
                .unwrap_or_default(),
        )
    }
}

fn pressure_delta(
    current: DotPressureCounters,
    previous: DotPressureCounters,
) -> DotPressureCounters {
    DotPressureCounters {
        block_events: current.block_events.saturating_sub(previous.block_events),
        total_blocked_us: current
            .total_blocked_us
            .saturating_sub(previous.total_blocked_us),
        dropped: current.dropped.saturating_sub(previous.dropped),
    }
}

impl GraphInner {
    pub(super) fn dot_interval(&self, now_us: i64, view: DotView) -> DotInterval {
        let nodes = self
            .nodes
            .iter()
            .map(|node| DotNodeCounters {
                processed: node.stats.processed.load(Ordering::Relaxed),
                errors: node.stats.errors.load(Ordering::Relaxed),
                total_us: node.stats.total_us.load(Ordering::Relaxed),
                packets_in: node.stats.packets_in.load(Ordering::Relaxed),
                packets_out: node.stats.packets_out.load(Ordering::Relaxed),
                latency_buckets: std::array::from_fn(|bucket| {
                    node.stats.latency_buckets[bucket].load(Ordering::Relaxed)
                }),
                cow_copies: node.stats.cow_copies.load(Ordering::Relaxed),
                cow_bytes: node.stats.cow_bytes.load(Ordering::Relaxed),
            })
            .collect();
        let ports = self
            .nodes
            .iter()
            .map(|node| {
                node.input_queue_stats
                    .iter()
                    .map(|stats| {
                        let since = stats.blocked_since_us.load(Ordering::Relaxed);
                        let active_us = if since == 0 {
                            0
                        } else {
                            now_us.saturating_sub(since.saturating_sub(1)).max(0) as u64
                        };
                        DotPressureCounters {
                            block_events: stats.block_events.load(Ordering::Relaxed),
                            total_blocked_us: stats
                                .blocked_total_us
                                .load(Ordering::Relaxed)
                                .saturating_add(active_us),
                            dropped: 0,
                        }
                    })
                    .collect()
            })
            .collect();
        let mut edges = Vec::with_capacity(self.edges.len());
        let mut pollers = Vec::with_capacity(self.edges.len());
        for edge in &self.edges {
            let watermark = edge.watermark_backpressure.snapshot(now_us);
            edges.push(DotPressureCounters {
                block_events: watermark.block_events,
                total_blocked_us: watermark.total_blocked_us,
                dropped: edge.dropped.load(Ordering::Relaxed),
            });
            pollers.push(
                edge.pollers
                    .lock()
                    .expect("poller list lock poisoned")
                    .iter()
                    .map(|poller| {
                        let pressure = poller.block_backpressure.snapshot(now_us);
                        DotPressureCounters {
                            block_events: pressure.block_events,
                            total_blocked_us: pressure.total_blocked_us,
                            dropped: poller.dropped.load(Ordering::Relaxed),
                        }
                    })
                    .collect(),
            );
        }
        let current = DotIntervalSnapshot {
            at_us: now_us,
            nodes,
            ports,
            edges,
            pollers,
        };
        let mut baselines = self
            .dot_intervals
            .lock()
            .expect("DOT interval lock poisoned");
        let baseline = match view {
            DotView::Compact => &mut baselines.compact,
            DotView::Diagnostics => &mut baselines.diagnostics,
            DotView::Topology => unreachable!("topology does not record interval baselines"),
        };
        let first = baseline.is_none();
        let previous = baseline.replace(current.clone()).unwrap_or_else(|| {
            let started = self.run_started_us.load(Ordering::Relaxed);
            DotIntervalSnapshot {
                at_us: if started == 0 {
                    now_us
                } else {
                    started.saturating_sub(1)
                },
                nodes: vec![DotNodeCounters::default(); current.nodes.len()],
                ports: current
                    .ports
                    .iter()
                    .map(|ports| vec![DotPressureCounters::default(); ports.len()])
                    .collect(),
                edges: vec![DotPressureCounters::default(); current.edges.len()],
                pollers: current
                    .pollers
                    .iter()
                    .map(|pollers| vec![DotPressureCounters::default(); pollers.len()])
                    .collect(),
            }
        });
        DotInterval {
            elapsed_us: now_us.saturating_sub(previous.at_us).max(0) as u64,
            first,
            current,
            previous,
        }
    }
}
