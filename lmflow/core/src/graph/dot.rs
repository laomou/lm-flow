//! Graphviz DOT 导出:拓扑 + 子图命名空间 cluster + 执行器/绑核图例,
//! 以及可选的运行统计标注与延迟热力图。
//!
//! 独立成模块的理由:这段纯粹是**读取快照并格式化字符串**,不参与调度、不碰锁序 ——
//! 与 `mod.rs` 里的并发核心放在一起只会让后者更难审。本模块是 `graph` 的子模块,
//! 故仍可访问 `Node` / `GraphInner` 的私有字段。

use super::{
    latency_bucket_upper_us, DotView, GraphInner, InputPolicy, NodeStats, LATENCY_BUCKETS,
};
use crate::config::StatsLevel;
use crate::dot::escape as escape_dot;
use std::sync::atomic::Ordering;

const NODE_LABEL_CHARS: usize = 24;
const KERNEL_LABEL_CHARS: usize = 28;
const PORT_LABEL_CHARS: usize = 28;
const CLUSTER_LABEL_CHARS: usize = 24;
const HOTSPOT_TOP_N: usize = 5;

#[derive(Debug, Clone, Copy, Default)]
struct DotNodeCounters {
    processed: u64,
    errors: u64,
    total_us: i64,
    packets_in: u64,
    packets_out: u64,
    latency_buckets: [u64; LATENCY_BUCKETS],
    cow_copies: u64,
    cow_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct DotPressureCounters {
    block_events: u64,
    total_blocked_us: u64,
    dropped: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DotIntervalSnapshot {
    at_us: i64,
    nodes: Vec<DotNodeCounters>,
    ports: Vec<Vec<DotPressureCounters>>,
    edges: Vec<DotPressureCounters>,
    pollers: Vec<Vec<DotPressureCounters>>,
}

#[derive(Debug, Default)]
pub(super) struct DotIntervalBaselines {
    compact: Option<DotIntervalSnapshot>,
    diagnostics: Option<DotIntervalSnapshot>,
}

#[derive(Debug, Clone)]
struct DotInterval {
    elapsed_us: u64,
    first: bool,
    current: DotIntervalSnapshot,
    previous: DotIntervalSnapshot,
}

impl DotInterval {
    fn node(&self, node: usize) -> DotNodeCounters {
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

    fn port(&self, node: usize, port: usize) -> DotPressureCounters {
        pressure_delta(
            self.current.ports[node][port],
            self.previous.ports[node][port],
        )
    }

    fn edge(&self, edge: usize) -> DotPressureCounters {
        pressure_delta(self.current.edges[edge], self.previous.edges[edge])
    }

    fn poller(&self, edge: usize, poller: usize) -> DotPressureCounters {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeRunState {
    Created,
    Idle,
    WaitingSource,
    Running,
    Closed,
    Error,
}

impl NodeRunState {
    fn label(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Idle => "IDLE",
            Self::WaitingSource => "WAITING_SOURCE",
            Self::Running => "RUNNING",
            Self::Closed => "CLOSED",
            Self::Error => "ERROR",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Created | Self::Closed => "#777777",
            Self::Idle => "#4c78a8",
            Self::WaitingSource => "#d6a700",
            Self::Running => "#2ca02c",
            Self::Error => "#d62728",
        }
    }

    fn sort_order(self) -> usize {
        match self {
            Self::Error => 0,
            Self::Running => 1,
            Self::WaitingSource => 2,
            Self::Idle => 3,
            Self::Created => 4,
            Self::Closed => 5,
        }
    }
}

#[derive(Default)]
struct DotHotspots {
    running: usize,
    errors: usize,
    blocked: usize,
    waiting: usize,
    dropped: u64,
}

#[derive(Debug, Default)]
struct DotAnalysis {
    node_ranks: Vec<Option<usize>>,
    port_ranks: std::collections::BTreeMap<(usize, usize), usize>,
    pressure_nodes: std::collections::BTreeSet<usize>,
    pressure_edges: std::collections::BTreeSet<usize>,
    top_nodes: Vec<String>,
    top_ports: Vec<String>,
}

impl DotAnalysis {
    fn summary(&self) -> String {
        let nodes = if self.top_nodes.is_empty() {
            "none".to_string()
        } else {
            self.top_nodes.join(", ")
        };
        let ports = if self.top_ports.is_empty() {
            "none".to_string()
        } else {
            self.top_ports.join(", ")
        };
        format!("top nodes {nodes}\\ntop ports {ports}")
    }
}

impl DotHotspots {
    fn label(&self) -> String {
        format!(
            "hotspots running {} · error {} · blocked {} · waiting {} · dropped {}",
            self.running, self.errors, self.blocked, self.waiting, self.dropped
        )
    }
}

fn truncate_label(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// 平均每次 process 耗时(µs);未跑过则 0。
fn avg_process_us(st: &NodeStats) -> f64 {
    let n = st.processed.load(Ordering::Relaxed);
    if n == 0 {
        0.0
    } else {
        st.total_us.load(Ordering::Relaxed) as f64 / n as f64
    }
}

fn duration_us(value: u64) -> String {
    if value < 1_000 {
        format!("{value}µs")
    } else if value < 1_000_000 {
        format!("{:.1}ms", value as f64 / 1_000.0)
    } else {
        format!("{:.2}s", value as f64 / 1_000_000.0)
    }
}

fn executor_load_color(saturated: bool, queued_for_us: u64, queued: usize) -> &'static str {
    if saturated && queued_for_us >= 1_000_000 {
        "#ffd6d6"
    } else if saturated || queued > 0 {
        "#ffe4b5"
    } else {
        "white"
    }
}

fn executor_queue_nodes_label(nodes: &[String]) -> String {
    if nodes.is_empty() {
        String::new()
    } else {
        format!("\\nqueue: {}", nodes.join(", "))
    }
}

fn executor_load_label(saturated: bool, queued_for_us: u64, queued: usize) -> String {
    if saturated {
        format!("\\nSATURATED · queued {}", duration_us(queued_for_us))
    } else if queued > 0 {
        format!("\\nBACKLOG · queued {}", duration_us(queued_for_us))
    } else {
        String::new()
    }
}

fn duration_us_f64(value: f64) -> String {
    duration_us(value.max(0.0).round().min(u64::MAX as f64) as u64)
}

fn latency_percentile_us(buckets: &[u64; LATENCY_BUCKETS], percentile: u64) -> Option<u64> {
    let total = buckets.iter().copied().sum::<u64>();
    if total == 0 {
        return None;
    }
    let target = total.saturating_mul(percentile).div_ceil(100).max(1);
    let mut cumulative = 0u64;
    for (bucket, count) in buckets.iter().copied().enumerate() {
        cumulative = cumulative.saturating_add(count);
        if cumulative >= target {
            return Some(latency_bucket_upper_us(bucket));
        }
    }
    Some(latency_bucket_upper_us(LATENCY_BUCKETS - 1))
}

fn byte_size(value: u64) -> String {
    if value < 1024 {
        format!("{value}B")
    } else if value < 1024 * 1024 {
        format!("{:.1}KiB", value as f64 / 1024.0)
    } else if value < 1024 * 1024 * 1024 {
        format!("{:.1}MiB", value as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2}GiB", value as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn rate_per_second(count: u64, elapsed_us: u64) -> String {
    if elapsed_us == 0 {
        return "0/s".to_string();
    }
    let rate = count as f64 * 1_000_000.0 / elapsed_us as f64;
    if rate < 10.0 {
        format!("{rate:.1}/s")
    } else {
        format!("{rate:.0}/s")
    }
}

/// 热力图配色:按 `v / max` 从绿(快)线性过渡到红(慢)。`max <= 0` 时返回白色。
fn heat_color(v: f64, max: f64) -> String {
    if max <= 0.0 {
        return "white".to_string();
    }
    let t = (v / max).clamp(0.0, 1.0);
    // 绿 (0xB7E1A1) → 红 (0xE88A7D),在 RGB 空间线性插值(够直观,不必上 HSL)
    let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    format!(
        "#{:02X}{:02X}{:02X}",
        lerp(0xB7, 0xE8),
        lerp(0xE1, 0x8A),
        lerp(0xA1, 0x7D)
    )
}

impl GraphInner {
    fn dot_analysis(&self, now_us: i64, interval: &DotInterval) -> DotAnalysis {
        let waiting = self
            .nodes
            .iter()
            .enumerate()
            .map(|(node, _)| self.dot_waiting_ports(node))
            .collect::<Vec<_>>();
        let mut port_candidates = Vec::new();
        for (node_id, node) in self.nodes.iter().enumerate() {
            for port in 0..node.input_queues.len() {
                let stats = self
                    .input_queue_stats_at(node_id, port, now_us)
                    .expect("node input port exists");
                let delta = interval.port(node_id, port);
                let edge_delta = interval.edge(node.inputs[port]);
                let severity = if stats.blocked {
                    5
                } else if waiting[node_id].contains(&port) {
                    4
                } else if edge_delta.dropped > 0 {
                    3
                } else if delta.block_events > 0 {
                    2
                } else if stats.queued_packets > 0 {
                    1
                } else {
                    0
                };
                if severity == 0 {
                    continue;
                }
                port_candidates.push((
                    (
                        severity,
                        delta.total_blocked_us,
                        edge_delta.dropped,
                        stats.queued_packets as u64,
                    ),
                    node_id,
                    port,
                ));
            }
        }
        port_candidates.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });

        let mut analysis = DotAnalysis {
            node_ranks: vec![None; self.nodes.len()],
            ..DotAnalysis::default()
        };
        for (index, (_, node, port)) in port_candidates.iter().take(HOTSPOT_TOP_N).enumerate() {
            let rank = index + 1;
            analysis.port_ranks.insert((*node, *port), rank);
            analysis.top_ports.push(format!(
                "#{rank} {}.{}",
                truncate_label(&self.nodes[*node].name, NODE_LABEL_CHARS),
                truncate_label(
                    self.nodes[*node].in_ports.name(*port).unwrap_or("?"),
                    PORT_LABEL_CHARS
                ),
            ));
        }

        let mut node_candidates = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(node_id, node)| {
                let delta = interval.node(node_id);
                let avg_us = if delta.processed == 0 {
                    avg_process_us(&node.stats).round() as u64
                } else {
                    (delta.total_us.max(0) as u64)
                        .checked_div(delta.processed)
                        .unwrap_or(0)
                };
                let blocked_ports = node
                    .input_queue_stats
                    .iter()
                    .filter(|stats| stats.blocked_since_us.load(Ordering::Relaxed) != 0)
                    .count() as u64;
                let waiting_ports = waiting[node_id].len() as u64;
                let queued = (0..node.input_queues.len())
                    .map(|port| node.queue_len(port))
                    .sum::<usize>() as u64;
                let outgoing_drops = node
                    .outputs
                    .iter()
                    .map(|edge| interval.edge(*edge).dropped)
                    .sum::<u64>();
                let severity = if delta.errors > 0 || node.stats.errors.load(Ordering::Relaxed) > 0
                {
                    5
                } else if blocked_ports > 0 {
                    4
                } else if waiting_ports > 0 || outgoing_drops > 0 {
                    3
                } else if avg_us > 0 && delta.processed > 0 {
                    2
                } else if queued > 0 {
                    1
                } else {
                    0
                };
                (severity > 0).then_some((
                    (
                        severity,
                        blocked_ports,
                        waiting_ports,
                        outgoing_drops,
                        avg_us,
                        queued,
                    ),
                    node_id,
                ))
            })
            .collect::<Vec<_>>();
        node_candidates
            .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        for (index, (_, node)) in node_candidates.iter().take(HOTSPOT_TOP_N).enumerate() {
            let rank = index + 1;
            analysis.node_ranks[*node] = Some(rank);
            analysis.top_nodes.push(format!(
                "#{rank} {}",
                truncate_label(&self.nodes[*node].name, NODE_LABEL_CHARS)
            ));
        }

        let active_roots = port_candidates
            .iter()
            .filter(|((severity, _, _, _), _, _)| *severity >= 4)
            .map(|(_, node, port)| (*node, *port))
            .collect::<Vec<_>>();
        let mut stack = active_roots
            .iter()
            .filter_map(|(node, port)| self.edges[self.nodes[*node].inputs[*port]].producer)
            .collect::<Vec<_>>();
        for (node, port) in active_roots {
            analysis
                .pressure_edges
                .insert(self.nodes[node].inputs[port]);
            analysis.pressure_nodes.insert(node);
        }
        while let Some(node) = stack.pop() {
            if !analysis.pressure_nodes.insert(node) {
                continue;
            }
            for &edge in &self.nodes[node].inputs {
                if self.edges[edge].is_graph_input {
                    analysis.pressure_edges.insert(edge);
                    continue;
                }
                if analysis.pressure_edges.insert(edge) {
                    if let Some(producer) = self.edges[edge].producer {
                        stack.push(producer);
                    }
                }
            }
        }
        analysis
    }

    fn dot_interval(&self, now_us: i64, view: DotView) -> DotInterval {
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
            DotView::Topology => unreachable!("topology has no statistics interval"),
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

    fn dot_node_state(&self, node_id: usize) -> NodeRunState {
        let node = &self.nodes[node_id];
        let (opened, closed) = {
            let sched = node.sched.lock().expect("scheduler lock poisoned");
            (sched.opened, sched.closed)
        };
        if node.stats.errors.load(Ordering::Relaxed) > 0 {
            NodeRunState::Error
        } else if node.stats.in_flight.load(Ordering::Relaxed) > 0 {
            NodeRunState::Running
        } else if node.is_source() && node.source_waiting.load(Ordering::SeqCst) {
            NodeRunState::WaitingSource
        } else if closed {
            NodeRunState::Closed
        } else if opened {
            NodeRunState::Idle
        } else {
            NodeRunState::Created
        }
    }

    fn dot_hotspots(&self, now_us: i64, node_states: &[NodeRunState]) -> DotHotspots {
        let mut hotspots = DotHotspots::default();
        for (node_id, node) in self.nodes.iter().enumerate() {
            match node_states[node_id] {
                NodeRunState::Running => hotspots.running += 1,
                NodeRunState::Error => hotspots.errors += 1,
                NodeRunState::WaitingSource => hotspots.waiting += 1,
                _ => {}
            }
            hotspots.blocked += node
                .input_queue_stats
                .iter()
                .filter(|stats| stats.blocked_since_us.load(Ordering::Relaxed) != 0)
                .count();
            hotspots.waiting += self.dot_waiting_ports(node_id).len();
        }
        for edge in &self.edges {
            if edge.is_graph_input && edge.watermark_backpressure.snapshot(now_us).blocked {
                hotspots.blocked += 1;
            }
            hotspots.dropped = hotspots
                .dropped
                .saturating_add(edge.dropped.load(Ordering::Relaxed));
            for poller in edge
                .pollers
                .lock()
                .expect("poller list lock poisoned")
                .iter()
            {
                if poller.block_backpressure.snapshot(now_us).blocked {
                    hotspots.blocked += 1;
                }
                hotspots.dropped = hotspots
                    .dropped
                    .saturating_add(poller.dropped.load(Ordering::Relaxed));
            }
        }
        hotspots
    }

    /// 导出 Graphviz DOT(`dot -Tsvg` 可渲染的拓扑图)。只读快照。
    ///
    /// - 节点按名字里的 `/`(子图展开留下的命名空间)还原成嵌套 cluster;
    /// - 节点填色 = 所在执行器,`@名` 标注(默认执行器是 `@default`);
    ///   委托执行器(交还宿主线程)留白底,线程池按序取色;
    /// - 图例列出各执行器的线程数、绑定核(亲和力)、实时优先级;
    /// - 边标注端口名;图输入/输出口画成独立形状。
    ///
    /// DOT id 用 `n{下标}` / `pin{边}` / `pout{边}`(纯下标,绝不撞名;人名一律进 label)。
    pub(super) fn to_dot(&self, view: DotView) -> String {
        let with_stats = view != DotView::Topology;
        let diagnostics = view == DotView::Diagnostics;
        let snapshot_us = with_stats.then(|| self.epoch_us());
        let interval = snapshot_us.map(|snapshot_us| self.dot_interval(snapshot_us, view));
        let analysis = snapshot_us.map(|snapshot_us| {
            self.dot_analysis(
                snapshot_us,
                interval.as_ref().expect("statistics interval exists"),
            )
        });
        let node_states = self
            .nodes
            .iter()
            .enumerate()
            .map(|(node_id, _)| self.dot_node_state(node_id))
            .collect::<Vec<_>>();
        // 执行器配色板(浅色填充);按执行器序号取模。
        const COLORS: &[&str] = &[
            "#cde4ff", "#d7f0d0", "#ffe4c7", "#f0d0e8", "#d0eeee", "#efe6b0", "#e0d4f0", "#ffd6d6",
        ];
        // 命名空间前缀树:内部结点 = cluster,叶子 = 图节点(存下标)。
        #[derive(Default)]
        struct Ns {
            children: std::collections::BTreeMap<String, Ns>,
            leaves: Vec<usize>,
        }
        impl Ns {
            fn insert(&mut self, path: &[&str], node: usize) {
                match path {
                    [] | [_] => self.leaves.push(node),
                    [head, rest @ ..] => self
                        .children
                        .entry((*head).to_string())
                        .or_default()
                        .insert(rest, node),
                }
            }
            fn emit(
                &self,
                lines: &[String],
                layout_keys: &[(usize, usize)],
                out: &mut String,
                cid: &mut usize,
            ) {
                let mut leaves = self.leaves.clone();
                leaves.sort_by_key(|&node| layout_keys[node]);
                for i in leaves {
                    out.push_str(&lines[i]);
                }
                for (name, child) in &self.children {
                    let id = *cid;
                    *cid += 1;
                    let label = truncate_label(name, CLUSTER_LABEL_CHARS);
                    out.push_str(&format!(
                        "  subgraph cluster_{id} {{ label=\"{}\"; tooltip=\"{}\"; style=dashed; color=\"#888888\";\n",
                        escape_dot(&label),
                        escape_dot(name),
                    ));
                    child.emit(lines, layout_keys, out, cid);
                    out.push_str("  }\n");
                }
            }
        }

        let mut out = crate::dot::begin("lmflow", None);
        if with_stats {
            let hotspots = self.dot_hotspots(
                snapshot_us.expect("statistics snapshot timestamp exists"),
                &node_states,
            );
            let limit = self.shared.config.max_queued_packets;
            let limit = if limit == 0 {
                "unbounded".to_string()
            } else {
                limit.to_string()
            };
            let started = self.run_started_us.load(Ordering::Relaxed);
            let snapshot = if started == 0 {
                "not started".to_string()
            } else {
                let elapsed = snapshot_us
                    .expect("statistics snapshot timestamp exists")
                    .saturating_sub(started.saturating_sub(1))
                    .max(0) as u64;
                format!("snapshot +{}", duration_us(elapsed))
            };
            let interval_label = interval.as_ref().map_or_else(
                || "window unavailable".to_string(),
                |interval| {
                    if interval.first {
                        format!("window since start {}", duration_us(interval.elapsed_us))
                    } else {
                        format!("window {}", duration_us(interval.elapsed_us))
                    }
                },
            );
            out.push_str(&format!(
                "  graph [labelloc=t, label=\"state {:?} · stats {} · {} · {} · queued {}/{} packets\\n{}\\n{}\"];\n",
                self.state(),
                self.stats_level.as_str(),
                snapshot,
                interval_label,
                self.shared.total_queued(),
                limit,
                hotspots.label(),
                analysis
                    .as_ref()
                    .expect("statistics analysis exists")
                    .summary(),
            ));
        }

        // 预渲染每个节点(短名 + kernel + 执行器,按执行器上色),并建命名空间树。
        // `with_stats` 时额外标出运行统计,并把填充色换成按平均延迟的热力图。
        let mut lines = vec![String::new(); self.nodes.len()];
        let mut layout_keys = vec![(0usize, 0usize); self.nodes.len()];
        let mut tree = Ns::default();
        // 热力图基准:全图最大平均延迟(0 则退化为不上色)。
        let max_avg_us = if with_stats && self.stats_level == StatsLevel::Full {
            self.nodes
                .iter()
                .enumerate()
                .map(|(node, n)| {
                    let delta = interval
                        .as_ref()
                        .expect("statistics interval exists")
                        .node(node);
                    if delta.processed == 0 {
                        avg_process_us(&n.stats)
                    } else {
                        delta.total_us.max(0) as f64 / delta.processed as f64
                    }
                })
                .fold(0.0f64, f64::max)
        } else {
            0.0
        };
        for (i, n) in self.nodes.iter().enumerate() {
            let short = n.name.rsplit('/').next().unwrap_or(n.name.as_str());
            let short_label = truncate_label(short, NODE_LABEL_CHARS);
            let kernel_label = truncate_label(&n.kernel_name, KERNEL_LABEL_CHARS);
            let kernel_language = n.kernel.language().label();
            let node_state = node_states[i];
            let executor_group = n.executor;
            layout_keys[i] = (executor_group, node_state.sort_order());
            let ex = &self.executors[n.executor];
            let exec = format!("@{}", ex.name());
            // 委托执行器留白底(它不拥有线程,和「有色的池」区分开);池按下标取色。
            let mut fill = if ex.is_delegating() {
                "white".to_string()
            } else {
                COLORS[n.executor % COLORS.len()].to_string()
            };
            let capacities = n
                .input_queue_capacity
                .iter()
                .enumerate()
                .map(|(port, capacity)| {
                    let value =
                        capacity.map_or_else(|| "unbounded".to_string(), |value| value.to_string());
                    format!(
                        "{}={value}",
                        truncate_label(n.in_ports.name(port).unwrap_or("?"), PORT_LABEL_CHARS)
                    )
                })
                .collect::<Vec<_>>();
            let mut extra = if capacities.is_empty() || with_stats {
                String::new()
            } else {
                format!("\\ncap {}", capacities.join(", "))
            };
            if with_stats {
                let st = &n.stats;
                let processed = st.processed.load(Ordering::Relaxed);
                let delta = interval
                    .as_ref()
                    .expect("statistics interval exists")
                    .node(i);
                let avg = if delta.processed == 0 {
                    avg_process_us(st)
                } else {
                    delta.total_us.max(0) as f64 / delta.processed as f64
                };
                let packets_in = st.packets_in.load(Ordering::Relaxed);
                let packets_out = st.packets_out.load(Ordering::Relaxed);
                let peak_queue_depth = st.peak_queue_depth.load(Ordering::Relaxed);
                let mut queued_bytes = 0u64;
                let mut peak_bytes = 0u64;
                let mut block_events = 0u64;
                let mut total_blocked_us = 0u64;
                let mut blocked_ports = 0usize;
                for port in 0..n.input_queues.len() {
                    let queue = &n.input_queue_stats[port];
                    queued_bytes = queued_bytes
                        .saturating_add(n.input_queue_bytes[port].load(Ordering::Relaxed));
                    peak_bytes =
                        peak_bytes.saturating_add(queue.peak_bytes.load(Ordering::Relaxed));
                    block_events =
                        block_events.saturating_add(queue.block_events.load(Ordering::Relaxed));
                    total_blocked_us = total_blocked_us
                        .saturating_add(queue.blocked_total_us.load(Ordering::Relaxed));
                    let since = queue.blocked_since_us.load(Ordering::Relaxed);
                    if since != 0 {
                        blocked_ports += 1;
                        let now_us = snapshot_us.expect("statistics snapshot timestamp exists");
                        total_blocked_us = total_blocked_us.saturating_add(
                            now_us.saturating_sub(since.saturating_sub(1)).max(0) as u64,
                        );
                    }
                }
                let errs = st.errors.load(Ordering::Relaxed);
                if let Some(rank) = analysis
                    .as_ref()
                    .expect("statistics analysis exists")
                    .node_ranks[i]
                {
                    extra.push_str(&format!("\\nHOT #{rank}"));
                }
                if analysis
                    .as_ref()
                    .expect("statistics analysis exists")
                    .pressure_nodes
                    .contains(&i)
                {
                    extra.push_str(" · PRESSURE PATH");
                }
                let active_or_abnormal = matches!(
                    node_state,
                    NodeRunState::Running | NodeRunState::WaitingSource | NodeRunState::Error
                ) || queued_bytes > 0
                    || block_events > 0
                    || blocked_ports > 0
                    || errs > 0;
                extra.push_str(&format!("\\n{}", node_state.label()));
                if node_state == NodeRunState::WaitingSource {
                    let reason = match n.source_wait_reason.load(Ordering::Relaxed) {
                        1 => "rate",
                        2 => "source_yield",
                        3 => "rate + source_yield",
                        _ => "scheduled wake",
                    };
                    let deadline = n.source_wake_deadline_us.load(Ordering::Relaxed);
                    let remaining = if deadline == 0 {
                        0
                    } else {
                        deadline
                            .saturating_sub(
                                snapshot_us.expect("statistics snapshot timestamp exists"),
                            )
                            .max(0) as u64
                    };
                    extra.push_str(&format!(
                        " · {} remaining · {}\\nyield {}×",
                        duration_us(remaining),
                        reason,
                        n.source_yield_count.load(Ordering::Relaxed),
                    ));
                }
                if self.stats_level != StatsLevel::Off
                    && (diagnostics || processed > 0 || active_or_abnormal)
                {
                    let timing = if self.stats_level == StatsLevel::Full {
                        format!(" · {} avg", duration_us_f64(avg))
                    } else {
                        " · timing n/a".to_string()
                    };
                    extra.push_str(&format!(
                        " · {} pkts (+{} · {}){}\\nin {} (+{}) / out {} (+{})",
                        processed,
                        delta.processed,
                        rate_per_second(
                            delta.processed,
                            interval
                                .as_ref()
                                .expect("statistics interval exists")
                                .elapsed_us,
                        ),
                        timing,
                        packets_in,
                        delta.packets_in,
                        packets_out,
                        delta.packets_out,
                    ));
                }
                if diagnostics && self.stats_level == StatsLevel::Full {
                    let histogram = if delta.latency_buckets.iter().any(|count| *count > 0) {
                        &delta.latency_buckets
                    } else {
                        &interval
                            .as_ref()
                            .expect("statistics interval exists")
                            .current
                            .nodes[i]
                            .latency_buckets
                    };
                    if let (Some(p50), Some(p95), Some(p99)) = (
                        latency_percentile_us(histogram, 50),
                        latency_percentile_us(histogram, 95),
                        latency_percentile_us(histogram, 99),
                    ) {
                        extra.push_str(&format!(
                            "\\nlat p50 {} · p95 {} · p99 {}",
                            duration_us(p50),
                            duration_us(p95),
                            duration_us(p99),
                        ));
                    }
                    let cow_copies = st.cow_copies.load(Ordering::Relaxed);
                    let cow_bytes = st.cow_bytes.load(Ordering::Relaxed);
                    if cow_copies > 0 {
                        extra.push_str(&format!(
                            "\\nCoW {}× / {} (+{}× / {})",
                            cow_copies,
                            byte_size(cow_bytes),
                            delta.cow_copies,
                            byte_size(delta.cow_bytes),
                        ));
                    }
                }
                if self.stats_level != StatsLevel::Off
                    && (diagnostics || peak_queue_depth > 0 || peak_bytes > 0)
                {
                    extra.push_str(&format!(" · peakQ {} / {}B", peak_queue_depth, peak_bytes,));
                }
                if queued_bytes > 0 || block_events > 0 || blocked_ports > 0 {
                    extra.push_str(&format!(
                        "\\nqueue {}B · bp {}× / {}",
                        queued_bytes,
                        block_events,
                        duration_us(total_blocked_us)
                    ));
                    if blocked_ports > 0 {
                        extra.push_str(&format!(" · {blocked_ports} blocked"));
                    }
                }
                if errs > 0 {
                    extra.push_str(&format!(" · {errs} err (+{})", delta.errors));
                }
                if diagnostics && !n.input_queues.is_empty() {
                    let waiting_ports = self.dot_waiting_ports(i);
                    extra.push_str("\\nports:");
                    for port in 0..n.input_queues.len() {
                        let queue = self
                            .input_queue_stats_at(
                                i,
                                port,
                                snapshot_us.expect("statistics snapshot timestamp exists"),
                            )
                            .expect("node input port exists");
                        let capacity = queue
                            .packet_capacity
                            .map_or_else(|| "∞".to_string(), |value| value.to_string());
                        let state = if queue.blocked {
                            " BLOCKED: queue full"
                        } else if waiting_ports.contains(&port) {
                            " WAITING: aligned input"
                        } else if queue.block_events > 0 {
                            " recovered: downstream slow"
                        } else {
                            ""
                        };
                        let rank = analysis
                            .as_ref()
                            .expect("statistics analysis exists")
                            .port_ranks
                            .get(&(i, port))
                            .map_or_else(String::new, |rank| format!(" HOT #{rank}"));
                        extra.push_str(&format!(
                            "\\n  {} {}/{} r{}{}{}",
                            escape_dot(&truncate_label(&queue.port_name, PORT_LABEL_CHARS)),
                            queue.queued_packets,
                            capacity,
                            queue.reserved_packets,
                            rank,
                            state,
                        ));
                    }
                }
                if self.stats_level == StatsLevel::Full {
                    // 按平均延迟上色:绿(快)→ 红(慢)。执行器配色让位给热力图。
                    fill = heat_color(avg, max_avg_us);
                }
            }
            let pressure_path = with_stats
                && analysis
                    .as_ref()
                    .expect("statistics analysis exists")
                    .pressure_nodes
                    .contains(&i);
            let state_color = if pressure_path
                && !matches!(node_state, NodeRunState::Running | NodeRunState::Error)
            {
                "#6f42c1"
            } else if with_stats {
                node_state.color()
            } else {
                "#333333"
            };
            let state_penwidth = if pressure_path
                || (with_stats && matches!(node_state, NodeRunState::Running | NodeRunState::Error))
            {
                3
            } else {
                1
            };
            lines[i] = format!(
                "  n{i} [label=\"{}\\n({} · {})\\n{}{}\", fillcolor=\"{}\", color=\"{}\", penwidth={}, group=\"exec{}\", tooltip=\"{}\"];\n",
                escape_dot(&short_label),
                escape_dot(&kernel_label),
                kernel_language,
                escape_dot(&exec),
                extra,
                fill,
                state_color,
                state_penwidth,
                executor_group,
                escape_dot(&format!(
                    "{} ({} · {}) on {}: state {}, stats {}, processed {}, avg {}, in {}, out {}, errors {}, CoW {} copies / {}, hotspot rank {}, pressure path {}",
                    n.name,
                    n.kernel_name,
                    kernel_language,
                    exec,
                    node_state.label(),
                    self.stats_level.as_str(),
                    n.stats.processed.load(Ordering::Relaxed),
                    if self.stats_level == StatsLevel::Full {
                        duration_us_f64(avg_process_us(&n.stats))
                    } else {
                        "n/a".to_string()
                    },
                    n.stats.packets_in.load(Ordering::Relaxed),
                    n.stats.packets_out.load(Ordering::Relaxed),
                    n.stats.errors.load(Ordering::Relaxed),
                    n.stats.cow_copies.load(Ordering::Relaxed),
                    byte_size(n.stats.cow_bytes.load(Ordering::Relaxed)),
                    analysis
                        .as_ref()
                        .and_then(|analysis| analysis.node_ranks[i])
                        .map_or_else(|| "none".to_string(), |rank| format!("#{rank}")),
                    analysis
                        .as_ref()
                        .is_some_and(|analysis| analysis.pressure_nodes.contains(&i)),
                )),
            );
            let path: Vec<&str> = n.name.split('/').collect();
            tree.insert(&path, i);
        }
        let mut cid = 0usize;
        tree.emit(&lines, &layout_keys, &mut out, &mut cid);

        // 图输入 / 输出口:独立形状。
        for &e in &self.graph_inputs {
            let mut label = escape_dot(&truncate_label(&self.edges[e].name, PORT_LABEL_CHARS));
            let mut fill = crate::dot::PORT_FILL;
            let mut color = crate::dot::PORT_COLOR;
            let mut penwidth = 1;
            let delta = interval.as_ref().map(|interval| interval.edge(e));
            let stats = diagnostics.then(|| {
                self.edges[e]
                    .watermark_backpressure
                    .snapshot(snapshot_us.expect("statistics snapshot timestamp exists"))
            });
            if let Some(stats) = &stats {
                label.push_str(&format!(
                    "\\ninput waits {}× / {}",
                    stats.block_events,
                    duration_us(stats.total_blocked_us),
                ));
                if let Some(delta) = delta {
                    label.push_str(&format!(
                        "\\nwindow +{}× / +{}",
                        delta.block_events,
                        duration_us(delta.total_blocked_us),
                    ));
                }
                if stats.blocked {
                    label.push_str(&format!(
                        "\\nBLOCKED: global packet limit {} · {} waiters",
                        duration_us(stats.blocked_for_us),
                        stats.active_waiters
                    ));
                    fill = "#ffd6d6";
                    color = "#d62728";
                    penwidth = 3;
                } else if stats.block_events > 0 {
                    fill = "#fff0c2";
                    color = "#d98c00";
                    penwidth = 2;
                }
            }
            let tooltip = stats.map_or_else(
                || format!("graph input {}", self.edges[e].name),
                |stats| {
                    format!(
                        "graph input {}: waits {}, total {}, active waiters {}",
                        self.edges[e].name,
                        stats.block_events,
                        duration_us(stats.total_blocked_us),
                        stats.active_waiters,
                    )
                },
            );
            out.push_str(&format!(
                "  pin{e} [shape=cds, style=filled, fillcolor=\"{fill}\", color=\"{color}\", penwidth={penwidth}, label=\"{label}\", tooltip=\"{}\"];\n",
                escape_dot(&tooltip),
            ));
        }
        for &e in &self.graph_outputs {
            out.push_str(&format!(
                "  pout{e} [shape=cds, style=filled, fillcolor=\"{}\", color=\"{}\", label=\"{}\", tooltip=\"graph output {}\"];\n",
                crate::dot::PORT_FILL,
                crate::dot::PORT_COLOR,
                escape_dot(&truncate_label(&self.edges[e].name, PORT_LABEL_CHARS)),
                escape_dot(&self.edges[e].name),
            ));
        }

        // 边:生产者 → 消费者;图输入口 → 消费者;生产者 → 图输出口。
        // 统计模式下,每个消费者输入口独立显示容量、积压、reservation 与背压状态。
        for (ei, e) in self.edges.iter().enumerate() {
            let label = escape_dot(&truncate_label(&e.name, PORT_LABEL_CHARS));
            if e.is_graph_input {
                for &(c, port) in &e.consumers {
                    let attrs = self.dot_edge_stats_attrs(
                        c,
                        port,
                        diagnostics
                            .then(|| snapshot_us.expect("statistics snapshot timestamp exists")),
                        interval.as_ref(),
                        analysis.as_ref(),
                    );
                    out.push_str(&format!("  pin{ei} -> n{c} [{attrs}];\n"));
                }
            } else if let Some(p) = e.producer {
                for &(c, port) in &e.consumers {
                    let attrs = self.dot_edge_stats_attrs(
                        c,
                        port,
                        diagnostics
                            .then(|| snapshot_us.expect("statistics snapshot timestamp exists")),
                        interval.as_ref(),
                        analysis.as_ref(),
                    );
                    out.push_str(&format!("  n{p} -> n{c} [{attrs}];\n"));
                }
            }
            if e.is_graph_output {
                if let Some(p) = e.producer {
                    out.push_str(&format!("  n{p} -> pout{ei} [label=\"{label}\"];\n"));
                }
            }
            if diagnostics {
                for (poller_index, poller) in e
                    .pollers
                    .lock()
                    .expect("poller list lock poisoned")
                    .iter()
                    .enumerate()
                {
                    let stats = poller
                        .block_backpressure
                        .snapshot(snapshot_us.expect("statistics snapshot timestamp exists"));
                    let queued = poller.queue.lock().expect("poller lock poisoned").len();
                    let dropped = poller.dropped.load(Ordering::Relaxed);
                    let delta = interval
                        .as_ref()
                        .expect("statistics interval exists")
                        .poller(ei, poller_index);
                    let capacity = poller
                        .capacity
                        .map_or_else(|| "unbounded".to_string(), |value| value.to_string());
                    let mut poller_label = format!(
                        "poller: {}\\n{:?} · queue {}/{}\\ndropped {} · bp {}× / {}",
                        label,
                        poller.overflow,
                        queued,
                        capacity,
                        dropped,
                        stats.block_events,
                        duration_us(stats.total_blocked_us),
                    );
                    poller_label.push_str(&format!(
                        "\\nwindow dropped +{} · bp +{}× / +{}",
                        delta.dropped,
                        delta.block_events,
                        duration_us(delta.total_blocked_us),
                    ));
                    let (fill, color, penwidth) = if stats.blocked {
                        poller_label.push_str(&format!(
                            "\\nBLOCKED: subscriber queue full {} · {} waiters",
                            duration_us(stats.blocked_for_us),
                            stats.active_waiters
                        ));
                        ("#ffd6d6", "#d62728", 3)
                    } else if dropped > 0 {
                        poller_label.push_str("\\nBOTTLENECK: subscriber dropping");
                        ("#fff0c2", "#d98c00", 2)
                    } else if stats.block_events > 0 {
                        poller_label.push_str("\\nBOTTLENECK: slow subscriber");
                        ("#fff0c2", "#d98c00", 2)
                    } else {
                        ("#e8f1fb", "#4c78a8", 1)
                    };
                    out.push_str(&format!(
                        "  poller{ei}_{poller_index} [shape=cylinder, style=filled, fillcolor=\"{fill}\", color=\"{color}\", penwidth={penwidth}, label=\"{poller_label}\", tooltip=\"poller {}: {:?}, queue {}/{}, dropped {}, block events {}, blocked total {}, active waiters {}\"];\n",
                        escape_dot(&e.name),
                        poller.overflow,
                        queued,
                        capacity,
                        dropped,
                        stats.block_events,
                        duration_us(stats.total_blocked_us),
                        stats.active_waiters,
                    ));
                    let source = if e.is_graph_output {
                        format!("pout{ei}")
                    } else if let Some(producer) = e.producer {
                        format!("n{producer}")
                    } else {
                        format!("pin{ei}")
                    };
                    out.push_str(&format!(
                        "  {source} -> poller{ei}_{poller_index} [style=dashed, color=\"{color}\", label=\"subscription\"];\n"
                    ));
                }
            }
        }

        // 执行器图例:拓扑模式填色 → 执行器;统计模式仍用标签标出 placement。
        // executors 恒非空(默认执行器一定在),故图例总会出现。
        {
            let legend_label = if with_stats {
                "executors (node label = placement)"
            } else {
                "executors (node fill = placement)"
            };
            out.push_str(&format!(
                "  subgraph cluster_legend {{\n    label=\"{legend_label}\"; style=dashed; color=\"#888888\";\n",
            ));
            for (i, ex) in self.executors.iter().enumerate() {
                // 委托执行器没有线程/绑核/优先级可言 —— 标出「交还宿主线程」而不是 0t。
                if ex.is_delegating() {
                    let stats = self
                        .executor_stats(ex.name())
                        .expect("executor exists while rendering DOT");
                    let fill = if with_stats {
                        executor_load_color(stats.saturated, stats.queued_for_us, stats.queued)
                    } else {
                        "white"
                    };
                    let runtime = if with_stats && self.stats_level == StatsLevel::Full {
                        format!(
                            "\\nqueued {} · running {}/1 · peak {} · done {}\\nwait {} · exec {}{}{}",
                            stats.queued,
                            stats.running,
                            stats.peak_queued,
                            stats.completed,
                            duration_us(stats.total_wait_us),
                            duration_us(stats.total_execution_us),
                            executor_load_label(
                                stats.saturated,
                                stats.queued_for_us,
                                stats.queued,
                            ),
                            executor_queue_nodes_label(&stats.queued_nodes),
                        )
                    } else if with_stats {
                        format!(
                            "\\nqueued {} · running {}/1 · detailed stats n/a{}",
                            stats.queued,
                            stats.running,
                            executor_queue_nodes_label(&stats.queued_nodes),
                        )
                    } else {
                        String::new()
                    };
                    out.push_str(&format!(
                        "    legend_e{i} [shape=box, style=filled, fillcolor=\"{fill}\", label=\"{}\\nhost thread (delegating){}\", tooltip=\"executor {}: queued {}, running {}/1, peak queued {}, completed {}, total wait {}, total execution {}, queued for {}, queued nodes {}\"];\n",
                        escape_dot(&truncate_label(ex.name(), NODE_LABEL_CHARS)),
                        runtime,
                        escape_dot(ex.name()),
                        stats.queued,
                        stats.running,
                        stats.peak_queued,
                        stats.completed,
                        duration_us(stats.total_wait_us),
                        duration_us(stats.total_execution_us),
                        duration_us(stats.queued_for_us),
                        escape_dot(&stats.queued_nodes.join(", ")),
                    ));
                    continue;
                }
                let cores = if ex.affinity().is_empty() {
                    "cores: any".to_string()
                } else {
                    format!(
                        "cores[{}]",
                        ex.affinity()
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                let prio = if ex.priority() > 0 {
                    format!(" · rt{}", ex.priority())
                } else {
                    String::new()
                };
                let stats = self
                    .executor_stats(ex.name())
                    .expect("executor exists while rendering DOT");
                let fill = if with_stats {
                    executor_load_color(stats.saturated, stats.queued_for_us, stats.queued)
                } else {
                    COLORS[i % COLORS.len()]
                };
                let runtime = if with_stats && self.stats_level == StatsLevel::Full {
                    format!(
                        "\\nqueued {} · running {}/{} · peak {} · done {}\\nwait {} · exec {}{}{}",
                        stats.queued,
                        stats.running,
                        ex.num_threads(),
                        stats.peak_queued,
                        stats.completed,
                        duration_us(stats.total_wait_us),
                        duration_us(stats.total_execution_us),
                        executor_load_label(stats.saturated, stats.queued_for_us, stats.queued),
                        executor_queue_nodes_label(&stats.queued_nodes),
                    )
                } else if with_stats {
                    format!(
                        "\\nqueued {} · running {}/{} · detailed stats n/a{}",
                        stats.queued,
                        stats.running,
                        ex.num_threads(),
                        executor_queue_nodes_label(&stats.queued_nodes),
                    )
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "    legend_e{i} [shape=box, style=filled, fillcolor=\"{}\", label=\"{}\\n{}t · {}{}{}\", tooltip=\"executor {}: queued {}, running {}/{}, peak queued {}, completed {}\"];\n",
                    fill,
                    escape_dot(&truncate_label(ex.name(), NODE_LABEL_CHARS)),
                    ex.num_threads(),
                    cores,
                    prio,
                    runtime,
                    escape_dot(ex.name()),
                    stats.queued,
                    stats.running,
                    ex.num_threads(),
                    stats.peak_queued,
                    stats.completed,
                ));
            }
            out.push_str("  }\n");
        }
        if with_stats {
            out.push_str(
                "  subgraph cluster_node_state_legend {\n    label=\"node state (border)\"; style=dashed; color=\"#888888\";\n",
            );
            out.push_str(
                "    legend_state_created [shape=box, style=filled, fillcolor=white, color=\"#777777\", label=\"CREATED\"];\n",
            );
            out.push_str(
                "    legend_state_idle [shape=box, style=filled, fillcolor=white, color=\"#4c78a8\", label=\"IDLE\"];\n",
            );
            out.push_str(
                "    legend_state_waiting_source [shape=box, style=filled, fillcolor=white, color=\"#d6a700\", penwidth=2, label=\"WAITING_SOURCE\"];\n",
            );
            out.push_str(
                "    legend_state_running [shape=box, style=filled, fillcolor=white, color=\"#2ca02c\", penwidth=3, label=\"RUNNING\"];\n",
            );
            out.push_str(
                "    legend_state_closed [shape=box, style=filled, fillcolor=white, color=\"#777777\", label=\"CLOSED\"];\n",
            );
            out.push_str(
                "    legend_state_error [shape=box, style=filled, fillcolor=white, color=\"#d62728\", penwidth=3, label=\"ERROR\"];\n",
            );
            out.push_str("  }\n");
        }
        if diagnostics {
            out.push_str(
                "  subgraph cluster_diagnostics_legend {\n    label=\"diagnostics\"; style=dashed; color=\"#888888\";\n",
            );
            out.push_str(
                "    legend_blocked [shape=box, style=filled, fillcolor=\"#ffd6d6\", color=\"#d62728\", penwidth=3, label=\"BLOCKED\\nproducer currently stalled\"];\n",
            );
            out.push_str(
                "    legend_waiting [shape=box, style=filled, fillcolor=\"#fff7bf\", color=\"#d6a700\", penwidth=2, label=\"WAITING\\nlikely missing aligned input\"];\n",
            );
            out.push_str(
                "    legend_history [shape=box, style=filled, fillcolor=\"#fff0c2\", color=\"#d98c00\", penwidth=2, label=\"recovered / dropped\\nhistorical pressure\"];\n",
            );
            out.push_str(
                "    legend_subscription [shape=box, style=\"rounded,filled\", fillcolor=\"#e8f1fb\", color=\"#4c78a8\", label=\"dashed edge\\npoller subscription\"];\n",
            );
            out.push_str(
                "    legend_hot [shape=box, style=filled, fillcolor=white, color=\"#333333\", penwidth=2, label=\"HOT #1..#5\\ncombined node / port rank\"];\n",
            );
            out.push_str(
                "    legend_pressure [shape=box, style=filled, fillcolor=white, color=\"#6f42c1\", fontcolor=\"#553096\", penwidth=3, label=\"PRESSURE PATH\\nupstream propagation to active stall\"];\n",
            );
            out.push_str(
                "    legend_executor_hot [shape=box, style=filled, fillcolor=\"#ffd6d6\", color=\"#d62728\", penwidth=2, label=\"EXECUTOR SATURATED\\nred >1s queued · orange shorter\"];\n",
            );
            out.push_str(
                "    legend_latency_cow [shape=box, style=filled, fillcolor=white, color=\"#555555\", label=\"lat p50/p95/p99\\nwindowed process latency\\nCoW copies / bytes\\nactual payload duplication\"];\n",
            );
            out.push_str("  }\n");
        }

        out.push_str("}\n");
        out
    }

    fn dot_edge_stats_attrs(
        &self,
        node: usize,
        port: usize,
        snapshot_us: Option<i64>,
        interval: Option<&DotInterval>,
        analysis: Option<&DotAnalysis>,
    ) -> String {
        let edge = self.nodes[node].inputs[port];
        let edge_name = &self.edges[edge].name;
        let edge_label = escape_dot(&truncate_label(edge_name, PORT_LABEL_CHARS));
        let Some(snapshot_us) = snapshot_us else {
            return format!(
                "label=\"{edge_label}\", tooltip=\"edge {}\"",
                escape_dot(edge_name)
            );
        };
        let stats = self
            .input_queue_stats_at(node, port, snapshot_us)
            .expect("consumer input port exists");
        let waiting = self.dot_waiting_ports(node).contains(&port);
        let delta = interval
            .expect("diagnostic edge has statistics interval")
            .port(node, port);
        let edge_delta = interval
            .expect("diagnostic edge has statistics interval")
            .edge(edge);
        let hotspot_rank = analysis
            .expect("diagnostic edge has analysis")
            .port_ranks
            .get(&(node, port))
            .copied();
        let pressure_path = analysis
            .expect("diagnostic edge has analysis")
            .pressure_edges
            .contains(&edge);
        let capacity = stats
            .packet_capacity
            .map_or_else(|| "unbounded".to_string(), |value| value.to_string());
        let mut label = format!(
            "{edge_label}\\nqueue {}/{} · reserved {}\\nbp {}× / {} · window +{}× / +{}",
            stats.queued_packets,
            capacity,
            stats.reserved_packets,
            stats.block_events,
            duration_us(stats.total_blocked_us),
            delta.block_events,
            duration_us(delta.total_blocked_us),
        );
        if edge_delta.dropped > 0 {
            label.push_str(&format!("\\ndropped +{} in window", edge_delta.dropped));
        }
        if let Some(rank) = hotspot_rank {
            label.push_str(&format!("\\nHOT #{rank}"));
        }
        let reason = if stats.blocked {
            "consumer queue full"
        } else if waiting {
            "missing aligned input"
        } else if edge_delta.dropped > 0 {
            "consumer cannot keep up"
        } else if delta.block_events > 0 || stats.block_events > 0 {
            "downstream drained slowly"
        } else {
            "healthy"
        };
        let tooltip = format!(
            "edge {} to {}.{}: reason {}, queue {}/{}, reserved {}, block events {}, blocked total {}, window block events {}, window blocked {}, window dropped {}, current blocked {}, producer {}",
            edge_name,
            stats.node_name,
            stats.port_name,
            reason,
            stats.queued_packets,
            capacity,
            stats.reserved_packets,
            stats.block_events,
            duration_us(stats.total_blocked_us),
            delta.block_events,
            duration_us(delta.total_blocked_us),
            edge_delta.dropped,
            duration_us(stats.blocked_for_us),
            stats.producer_name.as_deref().unwrap_or("graph input"),
        )
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
        if stats.blocked {
            label.push_str(&format!(
                "\\nBLOCKED: queue full {}",
                duration_us(stats.blocked_for_us)
            ));
            format!("label=\"{label}\", color=\"#d62728\", fontcolor=\"#a51414\", penwidth=3, tooltip=\"{tooltip}\"")
        } else if waiting {
            label.push_str("\\nWAITING: missing aligned input");
            format!("label=\"{label}\", color=\"#d6a700\", fontcolor=\"#806300\", penwidth=2, tooltip=\"{tooltip}\"")
        } else if edge_delta.dropped > 0 {
            label.push_str("\\nBOTTLENECK: consumer cannot keep up");
            format!("label=\"{label}\", color=\"#d98c00\", fontcolor=\"#9a6200\", penwidth=2, tooltip=\"{tooltip}\"")
        } else if stats.block_events > 0 {
            label.push_str("\\nRECOVERED: downstream drained slowly");
            format!("label=\"{label}\", color=\"#d98c00\", fontcolor=\"#9a6200\", penwidth=2, tooltip=\"{tooltip}\"")
        } else if pressure_path {
            label.push_str("\\nPRESSURE PATH: upstream propagation");
            format!("label=\"{label}\", color=\"#6f42c1\", fontcolor=\"#553096\", penwidth=3, tooltip=\"{tooltip}\"")
        } else {
            format!("label=\"{edge_label}\", tooltip=\"{tooltip}\"")
        }
    }

    fn dot_waiting_ports(&self, node_id: usize) -> std::collections::BTreeSet<usize> {
        let node = &self.nodes[node_id];
        let blocked_ports = (0..node.input_queues.len())
            .filter(|&port| {
                !node.input_is_back_edge[port]
                    && node.input_queue_stats[port]
                        .blocked_since_us
                        .load(Ordering::Relaxed)
                        != 0
            })
            .collect::<Vec<_>>();
        if blocked_ports.is_empty() {
            return std::collections::BTreeSet::new();
        }
        let is_empty_open = |port: usize| {
            !node.input_is_back_edge[port]
                && !node.input_closed[port].load(Ordering::Relaxed)
                && node.queue_len(port) == 0
        };
        match &node.policy {
            InputPolicy::Immediate => std::collections::BTreeSet::new(),
            InputPolicy::Sync | InputPolicy::FixedSize { .. } | InputPolicy::Batch { .. } => node
                .forward_ports()
                .filter(|&port| is_empty_open(port))
                .collect(),
            InputPolicy::SyncSet { sets } => sets
                .iter()
                .filter(|set| set.iter().any(|port| blocked_ports.contains(port)))
                .flat_map(|set| set.iter().copied())
                .filter(|&port| is_empty_open(port))
                .collect(),
        }
    }
}

#[cfg(test)]
mod percentile_tests {
    use super::*;

    #[test]
    fn percentile_uses_histogram_upper_bounds() {
        let mut buckets = [0u64; LATENCY_BUCKETS];
        buckets[0] = 5;
        buckets[3] = 4;
        buckets[10] = 1;
        assert_eq!(latency_percentile_us(&buckets, 50), Some(1));
        assert_eq!(latency_percentile_us(&buckets, 95), Some(1024));
        assert_eq!(latency_percentile_us(&buckets, 99), Some(1024));
        assert_eq!(latency_percentile_us(&[0; LATENCY_BUCKETS], 99), None);
    }
}
