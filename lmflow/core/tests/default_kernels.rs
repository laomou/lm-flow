//! 引擎自带默认 Rust 算子(`src/builtin.rs`)的端到端验收 —— **纯 Rust,零 C++**。
//!
//! 这些测试在两种 feature 配置下都跑(不带 `#![cfg]`),因为默认算子任何配置下都在。
//! 只有两个算子,且都是纯结构性的:`PassThrough`(接线)与 `Sink`(自行终结分支)。
//! 最后一条钉住「扇出是引擎原生能力」—— 正因如此才不需要 Split 算子。

use std::time::Duration;

use lmflow::{Graph, Packet, State, Timestamp};

#[test]
fn passthrough_forwards_unchanged() {
    let g = Graph::from_yaml(
        r#"
nodes:
  - { name: a, kernel: PassThrough, input_ports: ["in"], output_ports: ["mid"] }
  - { name: b, kernel: PassThrough, input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in 0..5i64 {
        inp.send(Packet::from_i64(i * 10).at(Timestamp(i))).unwrap();
    }
    g.close_all_inputs();
    let mut got = Vec::new();
    while got.len() < 5 {
        match out.next() {
            Some(p) => got.push(p.as_i64().unwrap()),
            None => break,
        }
    }
    assert_eq!(got, vec![0, 10, 20, 30, 40], "两级 PassThrough 应原样透传");
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(g.state(), State::Terminated);
}

#[test]
fn sink_consumes_and_counts() {
    // Sink 零输出口 —— 用**按图**计数器断言它真的收到了包并正常收尾。
    let g = Graph::from_yaml(
        r#"
nodes:
  - { name: k, kernel: Sink, input_ports: ["in"], output_ports: [] }
input_ports: ["in"]
output_ports: []
"#,
    )
    .unwrap();
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in 0..3i64 {
        inp.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
    }
    g.close_all_inputs();
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(g.counter_value("sink.packets"), 3);
    assert_eq!(g.counter_value("sink.closed"), 1);
    assert_eq!(g.state(), State::Terminated);
}

/// 两个默认算子都注册了。
#[test]
fn defaults_are_registered() {
    lmflow::builtin::register_defaults();
    let names = lmflow::kernel::registered_names();
    for want in ["PassThrough", "Sink"] {
        assert!(names.iter().any(|n| n == want), "{want} 应已注册:{names:?}");
    }
}

/// **扇出是引擎原生能力**:一条边可直接挂多个消费者,无需任何 Split 算子。
/// 这条是「默认算子里不放 Split」这个决定的依据。
#[test]
fn fanout_is_native_no_split_kernel_needed() {
    let g = Graph::from_yaml(
        r#"
nodes:
  - { name: a, kernel: PassThrough, input_ports: ["in"],  output_ports: ["mid"] }
  - { name: b, kernel: PassThrough, input_ports: ["mid"], output_ports: ["x"] }
  - { name: c, kernel: PassThrough, input_ports: ["mid"], output_ports: ["y"] }
input_ports: ["in"]
output_ports: ["x", "y"]
"#,
    )
    .unwrap();
    let px = g.add_poller("x").unwrap();
    let py = g.add_poller("y").unwrap();
    g.start().unwrap();
    g.input("in")
        .unwrap()
        .send(Packet::from_i64(7).at(Timestamp(0)))
        .unwrap();
    g.close_all_inputs();
    assert_eq!(px.next().unwrap().as_i64(), Some(7));
    assert_eq!(
        py.next().unwrap().as_i64(),
        Some(7),
        "同一条边的两个消费者都应收到"
    );
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();
}
