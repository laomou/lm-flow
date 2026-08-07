use std::process::Command;

fn write_config(contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "lmflow-config-cli-{}-{}.yaml",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn json_success_contains_expanded_topology() {
    let path = write_config(
        r#"
nodes:
  - { name: pass, kernel: NotLinkedYet, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_lmflow"))
        .args(["check-config", path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["nodes"][0]["kernel"], "NotLinkedYet");
    assert_eq!(value["edges"][0]["name"], "in");
    std::fs::remove_file(path).unwrap();
}

#[test]
fn json_failure_is_machine_readable() {
    let path = write_config(
        r#"
nodes:
  - { kernel: NotLinkedYet, input_ports: [missing] }
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_lmflow"))
        .args(["check-config", path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("nodes[0].input_ports[0]"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn dot_output_visualizes_static_plan_without_loading_kernels() {
    let path = write_config(
        r#"
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - name: source
    kernel: NotLinkedSource
    executor: cpu
    output_ports: [frames]
  - name: filter
    kernel: NotLinkedFilter
    executor: cpu
    input_ports: [frames, feedback]
    output_ports: [out, feedback]
    back_edges: [feedback]
    input_queues:
      packets: 8
      ports:
        frames: { packets: 2 }
input_ports: []
output_ports: [out]
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_lmflow"))
        .args(["check-config", path.to_str().unwrap(), "--dot"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let dot = String::from_utf8(output.stdout).unwrap();
    assert!(dot.contains("digraph lmflow_plan"), "{dot}");
    assert!(
        dot.contains("label=\"cpu · ThreadPoolExecutor · 2t\""),
        "{dot}"
    );
    assert!(dot.contains("NotLinkedFilter"), "{dot}");
    assert!(dot.contains("xlabel=\"queue 2 packets\""), "{dot}");
    assert!(dot.contains("style=dashed"), "{dot}");
    assert!(
        dot.contains("tooltip=\"back-edge latest-value register\""),
        "{dot}"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn dot_and_json_are_mutually_exclusive() {
    let path = write_config("nodes: []\n");
    let output = Command::new(env!("CARGO_BIN_EXE_lmflow"))
        .args(["check-config", path.to_str().unwrap(), "--dot", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("[--json|--dot]"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn json_includes_static_diagnostics_and_node_runtime_hints() {
    let path = write_config(
        r#"
executors:
  - { name: idle, type: ThreadPoolExecutor, num_threads: 3, affinity: [1, 3], priority: 7 }
nodes:
  - name: source
    kernel: NotLinked
    input_policy: { type: immediate }
    max_in_flight: 2
    rate: 30
    output_ports: [unused]
input_ports: [orphan]
output_ports: []
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_lmflow"))
        .args(["check-config", path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["nodes"][0]["input_policy"], "immediate");
    assert_eq!(value["nodes"][0]["max_in_flight"], 2);
    assert_eq!(value["nodes"][0]["rate_hz"], 30.0);
    assert_eq!(value["executors"][0]["affinity"], serde_json::json!([1, 3]));
    assert_eq!(value["executors"][0]["priority"], 7);
    let codes: Vec<_> = value["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"unconsumed_graph_input"));
    assert!(codes.contains(&"unconsumed_node_output"));
    assert!(codes.contains(&"unused_executor"));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn route_json_and_dot_share_condition_summary() {
    let path = write_config(
        r#"
nodes:
  - name: router
    type: route
    input_ports: [in]
    output_ports: [high, low]
    routes:
      - { to: high, when: { metadata: confidence, op: gte, value: 0.8 } }
      - { to: low, default: true }
input_ports: [in]
output_ports: [high, low]
"#,
    );
    let json_output = Command::new(env!("CARGO_BIN_EXE_lmflow"))
        .args(["check-config", path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(
        value["nodes"][0]["route"]["rules"][0]["when"],
        "confidence gte 0.8"
    );
    let dot_output = Command::new(env!("CARGO_BIN_EXE_lmflow"))
        .args(["check-config", path.to_str().unwrap(), "--dot"])
        .output()
        .unwrap();
    let dot = String::from_utf8(dot_output.stdout).unwrap();
    assert!(dot.contains("confidence gte 0.8"), "{dot}");
    std::fs::remove_file(path).unwrap();
}
