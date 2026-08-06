use lmflow::config::{GraphConfig, GraphPlan};

#[test]
fn preflight_does_not_require_kernel_registration() {
    let config = GraphConfig::preflight_from_yaml(
        r#"
nodes:
  - { name: pass, kernel: NotLinkedYet, output_ports: [out] }
output_ports: [out]
"#,
    )
    .unwrap();
    assert_eq!(config.nodes.len(), 1);
    assert_eq!(config.nodes[0].kernel, "NotLinkedYet");
}

#[test]
fn preflight_expands_subgraphs_and_clears_include() {
    let config = GraphConfig::preflight_from_yaml(
        r#"
nodes:
  - { name: stage, type: Tiny, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
subgraphs:
  Tiny:
    input_ports: [in]
    output_ports: [out]
    nodes:
      - { name: inner, kernel: NotLinkedYet, input_ports: [in], output_ports: [out] }
"#,
    )
    .unwrap();
    assert!(config.subgraphs.is_empty());
    assert_eq!(config.nodes[0].name, "stage/inner");
}

#[test]
fn preflight_rejects_invalid_topology_without_loading_kernels() {
    let error = GraphConfig::preflight_from_yaml(
        r#"
nodes:
  - { kernel: NotLinkedYet, input_ports: [missing] }
"#,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("nodes[0].input_ports[0]"),
        "{error}"
    );
}

#[test]
fn preflight_accepts_graph_input_as_graph_output() {
    GraphConfig::preflight_from_yaml(
        r#"
input_ports: [value]
output_ports: [value]
"#,
    )
    .unwrap();
}

#[test]
fn preflight_rejects_invalid_executor_and_sync_set() {
    let executor_error = GraphConfig::preflight_from_yaml(
        r#"
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { kernel: Source, executor: host, output_ports: [out] }
"#,
    )
    .unwrap_err();
    assert!(
        executor_error
            .to_string()
            .contains("source nodes cannot run"),
        "{executor_error}"
    );

    let policy_error = GraphConfig::preflight_from_yaml(
        r#"
nodes:
  - kernel: NotLinkedYet
    input_ports: [left, right]
    input_policy: { type: sync_set, sets: [[left]] }
input_ports: [left, right]
"#,
    )
    .unwrap_err();
    assert!(policy_error.to_string().contains("right"), "{policy_error}");
}

#[test]
fn graph_plan_matches_runtime_topology_shape() {
    let config = GraphConfig::from_yaml(
        r#"
nodes:
  - { name: first, kernel: NotLinkedYet, input_ports: [in], output_ports: [mid] }
  - { name: second, kernel: NotLinkedYet, input_ports: [mid], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let plan = GraphPlan::build(config).unwrap();
    assert_eq!(plan.nodes.len(), 2);
    assert_eq!(plan.edges.len(), 3);
    let mid = plan.edges.iter().find(|edge| edge.name == "mid").unwrap();
    assert_eq!(mid.producer, Some(0));
    assert_eq!(mid.consumers, vec![1]);
    assert_eq!(plan.nodes[1].inputs, vec!["mid"]);
}
