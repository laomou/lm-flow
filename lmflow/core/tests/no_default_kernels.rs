use lmflow::Graph;

#[test]
fn graph_build_does_not_install_default_kernels() {
    let error = Graph::from_yaml(
        r#"
nodes:
  - { name: pass, kernel: PassThrough, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("kernel `PassThrough` not registered"),
        "{error}"
    );
}
