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
        print!("{}", plan.to_dot());
    } else if json_output {
        println!("{}", summary_json(path, &plan));
    } else {
        print_summary(path, &plan);
    }
    ExitCode::SUCCESS
}

fn summary_json(path: &str, plan: &GraphPlan) -> serde_json::Value {
    let config = &plan.config;
    json!({
        "ok": true,
        "path": path,
        "nodes": config.nodes.iter().map(|node| json!({
            "name": node.name,
            "kernel": if node.r#type == "route" { "__lmflow.route" } else { &node.kernel },
            "type": node.r#type,
            "route": node.route.as_ref().map(|route| json!({
                "mode": format!("{:?}", route.mode).to_lowercase(),
                "unmatched": route.unmatched,
                "rules": route.routes.iter().map(|rule| json!({
                    "to": rule.to,
                    "default": rule.default,
                    "when": rule.when.as_ref().map(|predicate| predicate.summary()),
                })).collect::<Vec<_>>(),
            })),
            "executor": if node.executor.is_empty() { "default" } else { &node.executor },
            "inputs": node.input_ports,
            "outputs": node.output_ports,
            "input_policy": node.input_policy.r#type,
            "max_in_flight": node.max_in_flight,
            "rate_hz": node.rate,
            "input_queue_packets": node.input_queues.packets,
        })).collect::<Vec<_>>(),
        "diagnostics": plan.diagnostics().iter().map(|diagnostic| json!({
            "code": diagnostic.code,
            "message": diagnostic.message,
        })).collect::<Vec<_>>(),
        "executors": config.executors.iter().map(|executor| json!({
            "name": executor.name,
            "type": if executor.r#type.is_empty() {
                "ThreadPoolExecutor"
            } else {
                &executor.r#type
            },
            "threads": executor.num_threads,
            "affinity": executor.affinity,
            "priority": executor.priority,
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
