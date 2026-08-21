mod common;

use lmflow::Graph;

#[test]
fn missing_producer_reports_path_and_suggestion() {
    common::register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - name: sink
    kernel: Sink
    input_ports: [metdata]
input_ports: [metadata]
"#,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("nodes[0].input_ports[0]"), "{message}");
    assert!(message.contains("did you mean `metadata`"), "{message}");
}

#[test]
fn undefined_executor_reports_path_and_suggestion() {
    common::register_test_kernels();
    let error = Graph::from_yaml(
        r#"
executors:
  - { name: cpu_pool, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: source, kernel: PassThrough, executor: cpu_pol, output_ports: [out] }
output_ports: [out]
"#,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("nodes[0].executor"), "{message}");
    assert!(message.contains("did you mean `cpu_pool`"), "{message}");
}

#[test]
fn unknown_kernel_reports_path_and_suggestion() {
    common::register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - { name: source, kernel: PasThrough, output_ports: [out] }
output_ports: [out]
"#,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("nodes[0].kernel"), "{message}");
    assert!(message.contains("did you mean `PassThrough`"), "{message}");
}

#[test]
fn unknown_subgraph_reports_path_and_suggestion() {
    let error = Graph::from_yaml(
        r#"
nodes:
  - { name: stage, type: Deniose, input_ports: [in], output_ports: [out] }
subgraphs:
  Denoise:
    input_ports: [in]
    output_ports: [out]
    nodes: []
"#,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("nodes[0].type"), "{message}");
    assert!(message.contains("did you mean `Denoise`"), "{message}");
}

#[test]
fn included_yaml_parse_error_reports_file_and_path() {
    let root =
        std::env::temp_dir().join(format!("lmflow-config-diagnostics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let library = root.join("library.yaml");
    let main = root.join("main.yaml");
    std::fs::write(
        &library,
        "subgraphs:\n  Broken:\n    nodes:\n      - kernel: Sink\n        typo: true\n",
    )
    .unwrap();
    std::fs::write(&main, "include: [library.yaml]\nnodes: []\n").unwrap();

    let error = Graph::from_yaml_file(main.to_str().unwrap()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("include file `"), "{message}");
    assert!(message.contains("library.yaml"), "{message}");
    assert!(message.contains("subgraphs.Broken.nodes[0]"), "{message}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn subgraph_topology_error_reports_source_and_instance_path() {
    common::register_test_kernels();
    let error = Graph::from_yaml(
        r#"
subgraphs:
  Broken:
    input_ports: [in]
    nodes:
      - { name: sink, kernel: Sink, input_ports: [missing] }
nodes:
  - { name: stage, type: Broken, input_ports: [outer] }
input_ports: [outer]
"#,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("subgraphs.Broken.nodes[0]"), "{message}");
    assert!(message.contains("stage/sink"), "{message}");
}

#[test]
fn subgraph_unknown_kernel_reports_source_and_instance_path() {
    common::register_test_kernels();
    let error = Graph::from_yaml(
        r#"
subgraphs:
  Broken:
    input_ports: [in]
    nodes:
      - { name: bad, kernel: NoSuchKernelXYZ, input_ports: [in] }
nodes:
  - { name: stage, type: Broken, input_ports: [outer] }
input_ports: [outer]
"#,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("subgraphs.Broken.nodes[0]"), "{message}");
    assert!(message.contains("stage/bad"), "{message}");
    assert!(message.contains("NoSuchKernelXYZ"), "{message}");
}
