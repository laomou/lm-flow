use crate::graph::{latency_bucket_upper_us, NodeStats, LATENCY_BUCKETS};
use std::sync::atomic::Ordering;

pub(super) const NODE_LABEL_CHARS: usize = 24;
pub(super) const KERNEL_LABEL_CHARS: usize = 28;
pub(super) const PORT_LABEL_CHARS: usize = 28;
pub(super) const CLUSTER_LABEL_CHARS: usize = 24;
pub(super) const HOTSPOT_TOP_N: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeRunState {
    Created,
    Idle,
    WaitingSource,
    Running,
    Closed,
    Error,
}

impl NodeRunState {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Idle => "IDLE",
            Self::WaitingSource => "WAITING_SOURCE",
            Self::Running => "RUNNING",
            Self::Closed => "CLOSED",
            Self::Error => "ERROR",
        }
    }

    pub(super) fn color(self) -> &'static str {
        match self {
            Self::Created | Self::Closed => "#777777",
            Self::Idle => "#4c78a8",
            Self::WaitingSource => "#d6a700",
            Self::Running => "#2ca02c",
            Self::Error => "#d62728",
        }
    }

    pub(super) fn sort_order(self) -> usize {
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
pub(super) struct DotHotspots {
    pub(super) running: usize,
    pub(super) errors: usize,
    pub(super) blocked: usize,
    pub(super) waiting: usize,
    pub(super) dropped: u64,
}

#[derive(Debug, Default)]
pub(super) struct DotAnalysis {
    pub(super) node_ranks: Vec<Option<usize>>,
    pub(super) port_ranks: std::collections::BTreeMap<(usize, usize), usize>,
    pub(super) pressure_nodes: std::collections::BTreeSet<usize>,
    pub(super) pressure_edges: std::collections::BTreeSet<usize>,
    pub(super) top_nodes: Vec<String>,
    pub(super) top_ports: Vec<String>,
}

impl DotAnalysis {
    pub(super) fn summary(&self) -> String {
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
    pub(super) fn label(&self) -> String {
        format!(
            "hotspots running {} · error {} · blocked {} · waiting {} · dropped {}",
            self.running, self.errors, self.blocked, self.waiting, self.dropped
        )
    }
}

pub(super) fn truncate_label(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub(super) fn avg_process_us(st: &NodeStats) -> f64 {
    let n = st.processed.load(Ordering::Relaxed);
    if n == 0 {
        0.0
    } else {
        st.total_us.load(Ordering::Relaxed) as f64 / n as f64
    }
}

pub(super) fn duration_us(value: u64) -> String {
    if value < 1_000 {
        format!("{value}µs")
    } else if value < 1_000_000 {
        format!("{:.1}ms", value as f64 / 1_000.0)
    } else {
        format!("{:.2}s", value as f64 / 1_000_000.0)
    }
}

pub(super) fn executor_load_color(
    saturated: bool,
    queued_for_us: u64,
    queued: usize,
) -> &'static str {
    if saturated && queued_for_us >= 1_000_000 {
        "#ffd6d6"
    } else if saturated || queued > 0 {
        "#ffe4b5"
    } else {
        "white"
    }
}

pub(super) fn executor_queue_nodes_label(nodes: &[String]) -> String {
    if nodes.is_empty() {
        String::new()
    } else {
        format!("\\nqueue: {}", nodes.join(", "))
    }
}

pub(super) fn executor_load_label(saturated: bool, queued_for_us: u64, queued: usize) -> String {
    if saturated {
        format!("\\nSATURATED · queued {}", duration_us(queued_for_us))
    } else if queued > 0 {
        format!("\\nBACKLOG · queued {}", duration_us(queued_for_us))
    } else {
        String::new()
    }
}

pub(super) fn duration_us_f64(value: f64) -> String {
    duration_us(value.max(0.0).round().min(u64::MAX as f64) as u64)
}

pub(super) fn latency_percentile_us(
    buckets: &[u64; LATENCY_BUCKETS],
    percentile: u64,
) -> Option<u64> {
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

pub(super) fn byte_size(value: u64) -> String {
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

pub(super) fn rate_per_second(count: u64, elapsed_us: u64) -> String {
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

pub(super) fn heat_color(v: f64, max: f64) -> String {
    if max <= 0.0 {
        return "white".to_string();
    }
    let t = (v / max).clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    format!(
        "#{:02X}{:02X}{:02X}",
        lerp(0xB7, 0xE8),
        lerp(0xE1, 0x8A),
        lerp(0xA1, 0x7D)
    )
}
