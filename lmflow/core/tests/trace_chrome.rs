//! 逐次调用 trace 的端到端测试:开启 `trace_capacity` 跑一张真实小图,验证 span 被记录、
//! Chrome Trace JSON 结构合法、含各节点名与对齐时间戳;关闭时导出为合法的空 trace。

mod common;

use lmflow::{Graph, Packet, Timestamp};

/// 抽干输出 poller,顺带确保每个节点都 Process 过。
fn drain(graph: &Graph, port: &str, expect: usize) {
    let poller = graph.add_poller(port).unwrap();
    graph.start().unwrap();
    for i in 0..expect as i64 {
        graph
            .input("in")
            .unwrap()
            .send(Packet::from_i64(i).at(Timestamp(i)))
            .unwrap();
    }
    graph.close_all_inputs();
    let mut got = 0;
    while poller.next().is_some() {
        got += 1;
    }
    assert_eq!(got, expect, "all packets should reach the output");
}

#[test]
fn trace_capacity_records_spans_and_exports_chrome_json() {
    // in → mid(PassThrough) → tail(PassThrough) → out
    let graph = common::graph_from_yaml(
        r#"
trace_capacity: 256
nodes:
  - { name: mid,  kernel: PassThrough, input_ports: [in],      output_ports: [mid_out] }
  - { name: tail, kernel: PassThrough, input_ports: [mid_out], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    drain(&graph, "out", 3);

    let json = graph.to_chrome_trace();
    // 结构:Chrome Trace Event Format 的对象形式
    assert!(
        json.starts_with("{\"traceEvents\":["),
        "chrome trace shape: {json}"
    );
    assert!(json.contains("\"displayTimeUnit\""), "has display unit");
    // 两个节点的 span 都在(至少各自 Process 过)
    assert!(
        json.contains("\"name\":\"mid\""),
        "mid spans present: {json}"
    );
    assert!(
        json.contains("\"name\":\"tail\""),
        "tail spans present: {json}"
    );
    // complete 事件带 ts/dur;Process 阶段;带算子名与对齐时间戳
    assert!(json.contains("\"ph\":\"X\""), "complete events");
    assert!(json.contains("\"cat\":\"process\""), "process category");
    assert!(json.contains("\"kernel\":\"PassThrough\""), "kernel arg");
    assert!(json.contains("\"input_ts\":"), "input_ts arg");
    // 至少能找到一次带对齐时间戳 2 的 process(第 3 个包 Timestamp(2))
    assert!(
        json.contains("\"input_ts\":2"),
        "aligned ts recorded: {json}"
    );
}

#[test]
fn no_trace_capacity_yields_empty_trace() {
    let graph = common::graph_from_yaml(
        r#"
nodes:
  - { name: mid, kernel: PassThrough, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    drain(&graph, "out", 1);

    let json = graph.to_chrome_trace();
    assert!(
        json.contains("\"traceEvents\":[]"),
        "trace disabled → empty events: {json}"
    );
}
