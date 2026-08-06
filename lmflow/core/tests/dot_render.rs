mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use lmflow::{DotView, PollerOptions, PollerOverflow};

fn render_svg(dot: &str) -> Option<String> {
    let available = Command::new("dot").arg("-V").output().is_ok();
    if !available {
        assert!(
            std::env::var_os("LMFLOW_REQUIRE_GRAPHVIZ").is_none(),
            "Graphviz `dot` is required by this test run"
        );
        return None;
    }

    let mut child = Command::new("dot")
        .arg("-Tsvg")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Graphviz");
    child
        .stdin
        .take()
        .expect("Graphviz stdin")
        .write_all(dot.as_bytes())
        .expect("write DOT");
    let output = child.wait_with_output().expect("wait for Graphviz");
    assert!(
        output.status.success(),
        "Graphviz rejected DOT:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).expect("Graphviz SVG is UTF-8"))
}

#[test]
fn graphviz_renders_plain_and_diagnostic_svg() {
    let graph = common::graph_from_yaml(
        r#"
subgraphs:
  pair:
    nodes:
      - { name: pass, kernel: PassThrough, input_ports: [left, right], output_ports: [out] }
    input_ports: [left, right]
    output_ports: [out]
nodes:
  - { name: nested, type: pair, input_ports: [video, metadata], output_ports: [result] }
input_ports: [video, metadata]
output_ports: [result]
"#,
    )
    .unwrap();
    let _poller = graph
        .add_poller_with_options("result", PollerOptions::new(2, PollerOverflow::DropNewest))
        .unwrap();

    let Some(plain_svg) = render_svg(&graph.to_dot()) else {
        return;
    };
    assert!(plain_svg.contains("<svg"));
    assert!(plain_svg.contains("cluster_"));

    let compact_svg =
        render_svg(&graph.to_dot_with_view(DotView::Compact)).expect("Graphviz remains available");
    assert!(compact_svg.contains("<svg"));
    assert!(compact_svg.contains("node state (border)"));
    assert!(compact_svg.contains("CREATED"));
    assert!(!compact_svg.contains("diagnostics"));

    let stats_svg = render_svg(&graph.to_dot_with_stats()).expect("Graphviz remains available");
    assert!(stats_svg.contains("<svg"));
    assert!(stats_svg.contains("diagnostics"));
    assert!(stats_svg.contains("producer currently stalled"));
    assert!(stats_svg.contains("likely missing aligned input"));
    assert!(stats_svg.contains("poller: result"));
    assert!(
        stats_svg.contains("xlink:title") || stats_svg.contains("<title>"),
        "Graphviz SVG should retain hover details"
    );
}
