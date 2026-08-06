pub(crate) const FONT: &str = "sans";
pub(crate) const CLUSTER_COLOR: &str = "#888888";
pub(crate) const PORT_FILL: &str = "#e8e8e8";
pub(crate) const PORT_COLOR: &str = "#777777";
pub(crate) const BACK_EDGE_COLOR: &str = "#7b61a8";

pub(crate) fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn begin(name: &str, label: Option<&str>) -> String {
    let mut dot = format!(
        "digraph {name} {{\n  rankdir=LR;\n  newrank=true;\n  graph [fontname=\"{FONT}\", nodesep=0.35, ranksep=0.65"
    );
    if let Some(label) = label {
        dot.push_str(&format!(", labelloc=t, label=\"{}\"", escape(label)));
    }
    dot.push_str("];\n");
    dot.push_str(&format!(
        "  node [fontname=\"{FONT}\", shape=box, style=\"rounded,filled\", fillcolor=white, ordering=out];\n"
    ));
    dot.push_str(&format!("  edge [fontname=\"{FONT}\", fontsize=10];\n"));
    dot
}

pub(crate) fn executor_label(name: &str, kind: &str, threads: usize) -> String {
    if threads == 0 {
        format!("{name} · {kind}")
    } else {
        format!("{name} · {kind} · {threads}t")
    }
}

pub(crate) fn render_plan(plan: &crate::config::GraphPlan) -> String {
    let mut dot = begin("lmflow_plan", Some("lmflow configuration plan"));
    dot.push_str(&format!(
        "  graph_input [shape=cds, style=filled, fillcolor=\"{PORT_FILL}\", color=\"{PORT_COLOR}\", label=\"graph input\"];\n"
    ));
    dot.push_str(&format!(
        "  graph_output [shape=cds, style=filled, fillcolor=\"{PORT_FILL}\", color=\"{PORT_COLOR}\", label=\"graph output\"];\n"
    ));
    let mut executors = vec![("default".to_string(), "ThreadPoolExecutor".to_string(), 0)];
    executors.extend(plan.config.executors.iter().map(|executor| {
        (
            executor.name.clone(),
            if executor.r#type.is_empty() {
                "ThreadPoolExecutor".to_string()
            } else {
                executor.r#type.clone()
            },
            executor.num_threads,
        )
    }));
    for (index, (name, kind, threads)) in executors.iter().enumerate() {
        dot.push_str(&format!(
            "  subgraph cluster_executor_{index} {{\n    label=\"{}\";\n    color=\"{CLUSTER_COLOR}\";\n",
            escape(&executor_label(name, kind, *threads)),
        ));
        for node in plan.nodes.iter().filter(|node| node.executor == *name) {
            let label = format!(
                "{}\\n{}\\nexecutor: {}\\ninputs: {}\\noutputs: {}",
                node.name,
                node.kernel,
                node.executor,
                node.inputs.join(", "),
                node.outputs.join(", ")
            );
            dot.push_str(&format!(
                "    node_{} [label=\"{}\"];\n",
                node.index,
                escape(&label)
            ));
        }
        dot.push_str("  }\n");
    }
    for (edge_index, edge) in plan.edges.iter().enumerate() {
        let edge_id = format!("edge_{edge_index}");
        dot.push_str(&format!(
            "  {edge_id} [shape=point, width=0.08, label=\"\"];\n"
        ));
        if edge.graph_input {
            dot.push_str(&format!(
                "  graph_input -> {edge_id} [label=\"{}\"];\n",
                escape(&edge.name)
            ));
        }
        if let Some(producer) = edge.producer {
            dot.push_str(&format!("  node_{producer} -> {edge_id};\n"));
        }
        for consumer in &edge.consumers {
            let node = &plan.config.nodes[*consumer];
            let capacity = node
                .input_queues
                .ports
                .get(&edge.name)
                .and_then(|limits| limits.packets)
                .unwrap_or(node.input_queues.packets);
            let mut attributes = vec![format!("label=\"{}\"", escape(&edge.name))];
            if capacity != 0 {
                attributes.push(format!("xlabel=\"queue {capacity} packets\""));
            }
            if node.back_edges.contains(&edge.name) {
                attributes.extend([
                    "style=dashed".to_string(),
                    format!("color=\"{BACK_EDGE_COLOR}\""),
                    format!("fontcolor=\"{BACK_EDGE_COLOR}\""),
                    "constraint=false".to_string(),
                    "tooltip=\"back-edge latest-value register\"".to_string(),
                ]);
            }
            dot.push_str(&format!(
                "  {edge_id} -> node_{consumer} [{}];\n",
                attributes.join(", ")
            ));
        }
        if edge.graph_output {
            dot.push_str(&format!(
                "  {edge_id} -> graph_output [label=\"{}\"];\n",
                escape(&edge.name)
            ));
        }
    }
    dot.push_str("}\n");
    dot
}
