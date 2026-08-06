use std::env;
use std::process::ExitCode;

use lmflow::config::GraphConfig;
use lmflow::config::StatsLevel;
use serde_json::json;

fn usage() -> &'static str {
    "usage: lmflow check-config <graph.yaml> [--json]"
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 || args[0] != "check-config" {
        eprintln!("{usage}", usage = usage());
        return ExitCode::from(2);
    }
    let path = &args[1];
    let json_output = args[2..].iter().any(|arg| arg == "--json");
    if args[2..].iter().any(|arg| arg != "--json") {
        eprintln!("{usage}", usage = usage());
        return ExitCode::from(2);
    }

    let config = match GraphConfig::preflight_from_yaml_file(path) {
        Ok(config) => config,
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

    if json_output {
        println!("{}", summary_json(path, &config));
    } else {
        print_summary(path, &config);
    }
    ExitCode::SUCCESS
}

fn summary_json(path: &str, config: &GraphConfig) -> serde_json::Value {
    let edges = edge_summary(config);
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
        "edges": edges,
        "subgraphs_expanded": config.subgraphs.is_empty(),
        "stats": stats_name(config.effective_stats_level()),
    })
}

fn print_summary(path: &str, config: &GraphConfig) {
    println!("ok: {path}");
    println!(
        "nodes: {} | executors: {} | graph inputs: {} | graph outputs: {}",
        config.nodes.len(),
        config.executors.len(),
        config.input_ports.len(),
        config.output_ports.len()
    );
    for edge in edge_summary(config) {
        println!(
            "  edge {}: producer={} consumers={}",
            edge["name"].as_str().unwrap_or("?"),
            edge["producer"].as_str().unwrap_or("?"),
            edge["consumers"].as_array().map_or(0, Vec::len)
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

fn edge_summary(config: &GraphConfig) -> Vec<serde_json::Value> {
    let mut edges = std::collections::BTreeMap::<String, serde_json::Value>::new();
    for declaration in &config.input_ports {
        let name = port_name(declaration);
        edges.insert(
            name.clone(),
            json!({ "name": name, "producer": "graph", "consumers": [] }),
        );
    }
    for (node_index, node) in config.nodes.iter().enumerate() {
        for declaration in &node.output_ports {
            let name = port_name(declaration);
            edges.insert(
                name.clone(),
                json!({
                    "name": name,
                    "producer": format!("nodes[{node_index}]"),
                    "consumers": [],
                }),
            );
        }
    }
    for (node_index, node) in config.nodes.iter().enumerate() {
        for declaration in &node.input_ports {
            let name = port_name(declaration);
            if let Some(edge) = edges.get_mut(&name) {
                edge["consumers"]
                    .as_array_mut()
                    .expect("consumers is an array")
                    .push(json!(format!("nodes[{node_index}]")));
            }
        }
    }
    edges.into_values().collect()
}

fn port_name(declaration: &str) -> String {
    declaration
        .rsplit(':')
        .next()
        .unwrap_or(declaration)
        .to_string()
}

fn stats_name(level: StatsLevel) -> &'static str {
    match level {
        StatsLevel::Off => "off",
        StatsLevel::Basic => "basic",
        StatsLevel::Full => "full",
    }
}
