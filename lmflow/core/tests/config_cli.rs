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
