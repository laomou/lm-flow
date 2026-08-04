//! Graphviz DOT 导出:拓扑 + 子图命名空间 cluster + 执行器/绑核图例,
//! 以及可选的运行统计标注与延迟热力图。
//!
//! 独立成模块的理由:这段纯粹是**读取快照并格式化字符串**,不参与调度、不碰锁序 ——
//! 与 `mod.rs` 里的并发核心放在一起只会让后者更难审。本模块是 `graph` 的子模块,
//! 故仍可访问 `Node` / `GraphInner` 的私有字段。

use super::{GraphInner, NodeStats};
use std::sync::atomic::Ordering;

/// 平均每次 process 耗时(µs);未跑过则 0。
fn avg_process_us(st: &NodeStats) -> f64 {
    let n = st.processed.load(Ordering::Relaxed);
    if n == 0 {
        0.0
    } else {
        st.total_us.load(Ordering::Relaxed) as f64 / n as f64
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
    /// 导出 Graphviz DOT(`dot -Tsvg` 可渲染的拓扑图)。只读快照。
    ///
    /// - 节点按名字里的 `/`(子图展开留下的命名空间)还原成嵌套 cluster;
    /// - 节点填色 = 所在执行器(线程池),`@名` 标注;`@main` = 宿主主线程;
    /// - 图例列出各执行器的线程数、绑定核(亲和力)、实时优先级;
    /// - 边标注端口名;图输入/输出口画成独立形状。
    ///
    /// DOT id 用 `n{下标}` / `pin{边}` / `pout{边}`(纯下标,绝不撞名;人名一律进 label)。
    pub(super) fn to_dot(&self, with_stats: bool) -> String {
        // 执行器配色板(浅色填充);按执行器序号取模。
        const COLORS: &[&str] = &[
            "#cde4ff", "#d7f0d0", "#ffe4c7", "#f0d0e8", "#d0eeee", "#efe6b0", "#e0d4f0", "#ffd6d6",
        ];
        // DOT 字符串转义:反斜杠与引号。
        fn esc(s: &str) -> String {
            s.replace('\\', "\\\\").replace('"', "\\\"")
        }
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
            fn emit(&self, lines: &[String], out: &mut String, cid: &mut usize) {
                for &i in &self.leaves {
                    out.push_str(&lines[i]);
                }
                for (name, child) in &self.children {
                    let id = *cid;
                    *cid += 1;
                    out.push_str(&format!(
                        "  subgraph cluster_{id} {{ label=\"{}\"; style=dashed; color=\"#888888\";\n",
                        esc(name)
                    ));
                    child.emit(lines, out, cid);
                    out.push_str("  }\n");
                }
            }
        }

        let mut out = String::new();
        out.push_str("digraph lmflow {\n");
        out.push_str("  rankdir=LR;\n");
        out.push_str("  node [shape=box, style=\"rounded,filled\", fillcolor=white];\n");
        out.push_str("  edge [fontsize=10];\n");

        // 预渲染每个节点(短名 + kernel + 执行器,按执行器上色),并建命名空间树。
        // `with_stats` 时额外标出运行统计,并把填充色换成按平均延迟的热力图。
        let mut lines = vec![String::new(); self.nodes.len()];
        let mut tree = Ns::default();
        // 热力图基准:全图最大平均延迟(0 则退化为不上色)。
        let max_avg_us = if with_stats {
            self.nodes
                .iter()
                .map(|n| avg_process_us(&n.stats))
                .fold(0.0f64, f64::max)
        } else {
            0.0
        };
        for (i, n) in self.nodes.iter().enumerate() {
            let short = n.name.rsplit('/').next().unwrap_or(n.name.as_str());
            let (exec, mut fill) = match n.executor {
                None => ("@main".to_string(), "white".to_string()),
                Some(ei) => (
                    format!("@{}", self.executors[ei].name()),
                    COLORS[ei % COLORS.len()].to_string(),
                ),
            };
            let capacities = n
                .input_queue_capacity
                .iter()
                .enumerate()
                .map(|(port, capacity)| {
                    let value =
                        capacity.map_or_else(|| "unbounded".to_string(), |value| value.to_string());
                    format!("{}={value}", n.in_ports.name(port).unwrap_or("?"))
                })
                .collect::<Vec<_>>();
            let mut extra = if capacities.is_empty() {
                String::new()
            } else {
                format!("\\ncap {}", capacities.join(", "))
            };
            if with_stats {
                let st = &n.stats;
                let processed = st.processed.load(Ordering::Relaxed);
                let avg = avg_process_us(st);
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
                        let now = self.epoch.elapsed().as_micros().min(i64::MAX as u128) as i64;
                        total_blocked_us = total_blocked_us.saturating_add(
                            now.saturating_sub(since.saturating_sub(1)).max(0) as u64,
                        );
                    }
                }
                extra.push_str(&format!(
                    "\\n{} pkts · {:.0}µs avg\\nin {} / out {} · peakQ {} / {}B",
                    processed,
                    avg,
                    st.packets_in.load(Ordering::Relaxed),
                    st.packets_out.load(Ordering::Relaxed),
                    st.peak_queue_depth.load(Ordering::Relaxed),
                    peak_bytes,
                ));
                if queued_bytes > 0 || block_events > 0 || blocked_ports > 0 {
                    extra.push_str(&format!(
                        "\\nqueue {}B · bp {}× / {}µs",
                        queued_bytes, block_events, total_blocked_us
                    ));
                    if blocked_ports > 0 {
                        extra.push_str(&format!(" · {blocked_ports} blocked"));
                    }
                }
                let errs = st.errors.load(Ordering::Relaxed);
                if errs > 0 {
                    extra.push_str(&format!(" · {errs} err"));
                }
                // 按平均延迟上色:绿(快)→ 红(慢)。执行器配色让位给热力图。
                fill = heat_color(avg, max_avg_us);
            }
            lines[i] = format!(
                "  n{i} [label=\"{}\\n({})\\n{}{}\", fillcolor=\"{}\"];\n",
                esc(short),
                esc(&n.kernel_name),
                esc(&exec),
                extra,
                fill
            );
            let path: Vec<&str> = n.name.split('/').collect();
            tree.insert(&path, i);
        }
        let mut cid = 0usize;
        tree.emit(&lines, &mut out, &mut cid);

        // 图输入 / 输出口:独立形状。
        for &e in &self.graph_inputs {
            out.push_str(&format!(
                "  pin{e} [shape=cds, style=filled, fillcolor=\"#e8e8e8\", label=\"{}\"];\n",
                esc(&self.edges[e].name)
            ));
        }
        for &e in &self.graph_outputs {
            out.push_str(&format!(
                "  pout{e} [shape=cds, style=filled, fillcolor=\"#e8e8e8\", label=\"{}\"];\n",
                esc(&self.edges[e].name)
            ));
        }

        // 边:生产者 → 消费者;图输入口 → 消费者;生产者 → 图输出口。label = 端口名。
        for (ei, e) in self.edges.iter().enumerate() {
            let label = esc(&e.name);
            if e.is_graph_input {
                for &(c, _) in &e.consumers {
                    out.push_str(&format!("  pin{ei} -> n{c} [label=\"{label}\"];\n"));
                }
            } else if let Some(p) = e.producer {
                for &(c, _) in &e.consumers {
                    out.push_str(&format!("  n{p} -> n{c} [label=\"{label}\"];\n"));
                }
            }
            if e.is_graph_output {
                if let Some(p) = e.producer {
                    out.push_str(&format!("  n{p} -> pout{ei} [label=\"{label}\"];\n"));
                }
            }
        }

        // 执行器图例:填色 → 线程池,列线程数 / 绑核 / 优先级。
        if !self.executors.is_empty() {
            out.push_str(
                "  subgraph cluster_legend {\n    label=\"executors (node fill = placement)\"; style=dashed; color=\"#888888\";\n",
            );
            for (i, ex) in self.executors.iter().enumerate() {
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
                out.push_str(&format!(
                    "    legend_e{i} [shape=box, style=filled, fillcolor=\"{}\", label=\"{}\\n{}t · {}{}\"];\n",
                    COLORS[i % COLORS.len()],
                    esc(ex.name()),
                    ex.num_threads(),
                    cores,
                    prio
                ));
            }
            out.push_str(
                "    legend_main [shape=box, style=filled, fillcolor=white, label=\"main thread\\n(default)\"];\n",
            );
            out.push_str("  }\n");
        }

        out.push_str("}\n");
        out
    }
}
