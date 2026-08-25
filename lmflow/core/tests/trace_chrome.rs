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

/// 导出的东西**必须真能被解析** —— 上面那些 `contains` 断言对「少一个逗号 / 多一个括号」
/// 完全无感,而查看器打不开的 trace 一文不值。故这里真解析一遍并按结构断言。
#[test]
fn exported_trace_is_parseable_json_with_named_lanes_and_all_phases() {
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
    // Close 阶段的 span 要等图收尾;drain 已 close_all_inputs 并抽干。
    graph.wait_done().ok();

    let json = graph.to_chrome_trace();
    let root: serde_json::Value =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("导出的不是合法 JSON: {e}\n{json}"));
    let events = root["traceEvents"]
        .as_array()
        .unwrap_or_else(|| panic!("traceEvents 应为数组: {json}"));
    assert!(!events.is_empty(), "应记到 span");

    // 泳道命名:至少一条 thread_name 元事件,否则查看器只显示裸 tid。
    let named_lanes = events
        .iter()
        .filter(|e| e["name"] == "thread_name" && e["ph"] == "M")
        .count();
    assert!(named_lanes >= 1, "应有泳道命名元事件: {json}");

    // 每条 complete 事件的必填字段都得在,且类型对得上。
    let mut phases = std::collections::BTreeSet::new();
    for e in events.iter().filter(|e| e["ph"] == "X") {
        assert!(e["name"].is_string(), "span 需要 name: {e}");
        assert!(e["ts"].is_i64(), "span 需要数值 ts: {e}");
        assert!(e["dur"].is_i64(), "span 需要数值 dur: {e}");
        assert!(e["dur"].as_i64().unwrap() >= 0, "dur 不得为负: {e}");
        assert!(e["tid"].is_u64(), "span 需要数值 tid: {e}");
        phases.insert(e["cat"].as_str().expect("cat 应为字符串").to_string());

        // input_ts 要么是流内数值,要么是哨兵名字 —— 不能是贴着 i64::MIN/MAX 的天文数字。
        let ts = &e["args"]["input_ts"];
        if let Some(n) = ts.as_i64() {
            assert!(
                n > i64::MIN + 3 && n < i64::MAX - 3,
                "流内时间戳不该是哨兵原值: {e}"
            );
        } else {
            assert!(ts.is_string(), "input_ts 应为数值或哨兵名: {e}");
        }
    }

    // PR 声称 Open/Process/Close 三个阶段都记 —— 这里把它钉住。
    for want in ["open", "process", "close"] {
        assert!(
            phases.contains(want),
            "应记到 {want} 阶段, 实际只有 {phases:?}"
        );
    }
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
