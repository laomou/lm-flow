use std::env;
use std::process::ExitCode;

use lmflow::config::StatsLevel;
use lmflow::config::{GraphConfig, GraphPlan};
use serde_json::json;

fn usage() -> &'static str {
    "usage: lmflow check-config <graph.yaml> [--json|--dot]"
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 || args[0] != "check-config" {
        eprintln!("{usage}", usage = usage());
        return ExitCode::from(2);
    }
    let path = &args[1];
    let json_output = args[2..].iter().any(|arg| arg == "--json");
    let dot_output = args[2..].iter().any(|arg| arg == "--dot");
    if args[2..]
        .iter()
        .any(|arg| arg != "--json" && arg != "--dot")
        || (json_output && dot_output)
    {
        eprintln!("{usage}", usage = usage());
        return ExitCode::from(2);
    }

    let plan = match GraphConfig::from_yaml_file(path).and_then(GraphPlan::build) {
        Ok(plan) => plan,
        Err(error) => {
            if json_output {
                println!(
                    "{}",
                    json!({
                        "ok": false,
                        "path": path,
                        "diagnostics": [{
                            "message": error.to_string(),
                            "code": error.code(),
                        }],
                    })
                );
            } else {
                eprintln!("check-config: {error}");
            }
            return ExitCode::from(1);
        }
    };

    if dot_output {
        print!("{}", plan_dot(&plan));
    } else if json_output {
        println!("{}", summary_json(path, &plan));
    } else {
        print_summary(path, &plan);
    }
    ExitCode::SUCCESS
}

fn plan_dot(plan: &GraphPlan) -> String {
    let mut dot = String::from(
        "digraph lmflow_plan {\n  rankdir=LR;\n  graph [fontname=\"sans\", labelloc=\"t\", label=\"lmflow configuration plan\"];\n  node [fontname=\"sans\"];\n  edge [fontname=\"sans\"];\n",
    );
    dot.push_str("  graph_input [shape=cds, label=\"graph input\"];\n");
    dot.push_str("  graph_output [shape=cds, label=\"graph output\"];\n");

    let mut executors = vec![(
        "default".to_string(),
        "ThreadPoolExecutor".to_string(),
        0usize,
    )];
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
            "  subgraph cluster_executor_{index} {{\n    label=\"{} · {}{}\";\n    color=\"#bdbdbd\";\n",
            escape_dot(name),
            escape_dot(kind),
            if *threads == 0 {
                String::new()
            } else {
                format!(" · {threads}t")
            }
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
                "    node_{} [shape=box, label=\"{}\"];\n",
                node.index,
                escape_dot(&label)
            ));
        }
        dot.push_str("  }\n");
    }
    for (edge_index, edge) in plan.edges.iter().enumerate() {
        let edge_id = format!("edge_{edge_index}");
        if edge.graph_input {
            dot.push_str(&format!(
                "  graph_input -> {} [label=\"{}\"];\n",
                edge_id,
                escape_dot(&edge.name)
            ));
        }
        dot.push_str(&format!(
            "  {} [shape=point, width=0.08, label=\"\"];\n",
            edge_id
        ));
        if let Some(producer) = edge.producer {
            dot.push_str(&format!("  node_{producer} -> {edge_id};\n"));
        }
        for consumer in &edge.consumers {
            let node = &plan.config.nodes[*consumer];
            let back_edge = node.back_edges.contains(&edge.name);
            let capacity = node
                .input_queues
                .ports
                .get(&edge.name)
                .and_then(|limits| limits.packets)
                .unwrap_or(node.input_queues.packets);
            let mut attributes = vec![format!("label=\"{}\"", escape_dot(&edge.name))];
            if capacity != 0 {
                attributes.push(format!("xlabel=\"queue {capacity} packets\""));
            }
            if back_edge {
                attributes.push("style=dashed".to_string());
                attributes.push("color=\"#7b61a8\"".to_string());
                attributes.push("fontcolor=\"#7b61a8\"".to_string());
                attributes.push("constraint=false".to_string());
                attributes.push("tooltip=\"back-edge latest-value register\"".to_string());
            }
            dot.push_str(&format!(
                "  {edge_id} -> node_{consumer} [{}];\n",
                attributes.join(", ")
            ));
        }
        if edge.graph_output {
            dot.push_str(&format!(
                "  {} -> graph_output [label=\"{}\"];\n",
                edge_id,
                escape_dot(&edge.name)
            ));
        }
    }
    dot.push_str("}\n");
    dot
}

fn escape_dot(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn summary_json(path: &str, plan: &GraphPlan) -> serde_json::Value {
    let config = &plan.config;
    json!({
        "ok": true,
        "path": path,
        "nodes": config.nodes.iter().map(|node| json!({
            "name": node.name,
            "kernel": node.kernel,
            "executor": if node.executor.is_empty() { "default" } else { &node.executor },
            "inputs": node.input_ports,
            "outputs": node.output_ports,
        })).collect::<Vec<_>>(),
        "executors": config.executors.iter().map(|executor| json!({
            "name": executor.name,
            "type": if executor.r#type.is_empty() {
                "ThreadPoolExecutor"
            } else {
                &executor.r#type
            },
            "threads": executor.num_threads,
        })).collect::<Vec<_>>(),
        "graph_inputs": config.input_ports,
        "graph_outputs": config.output_ports,
        "edges": plan.edges.iter().map(|edge| json!({
            "name": edge.name,
            "producer": edge.producer.map(|index| format!("nodes[{index}]")).unwrap_or_else(|| "graph".into()),
            "consumers": edge.consumers.iter().map(|index| format!("nodes[{index}]")).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "subgraphs_expanded": config.subgraphs.is_empty(),
        "stats": stats_name(config.effective_stats_level()),
    })
}

fn print_summary(path: &str, plan: &GraphPlan) {
    let config = &plan.config;
    println!("ok: {path}");
    println!(
        "nodes: {} | executors: {} | graph inputs: {} | graph outputs: {}",
        config.nodes.len(),
        config.executors.len(),
        config.input_ports.len(),
        config.output_ports.len()
    );
    for edge in &plan.edges {
        println!(
            "  edge {}: producer={} consumers={}",
            edge.name,
            edge.producer
                .map(|index| format!("nodes[{index}]"))
                .unwrap_or_else(|| "graph".into()),
            edge.consumers.len()
        );
    }
    for (index, node) in config.nodes.iter().enumerate() {
        let name = if node.name.is_empty() {
            "<unnamed>"
        } else {
            &node.name
        };
        let executor = if node.executor.is_empty() {
            "default"
        } else {
            &node.executor
        };
        println!(
            "  node[{index}] {name}: kernel={} executor={} inputs={} outputs={}",
            node.kernel,
            executor,
            node.input_ports.len(),
            node.output_ports.len()
        );
    }
    println!("configuration-only preflight: no executors or kernel instances created");
}

fn stats_name(level: StatsLevel) -> &'static str {
    match level {
        StatsLevel::Off => "off",
        StatsLevel::Basic => "basic",
        StatsLevel::Full => "full",
    }
}
