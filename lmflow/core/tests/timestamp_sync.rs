//! 多输入口的时间戳同步(阶段 A 的核心)。
//!
//! 没有对齐时,`Zip` 之类的算子会把不同时刻的数据配到一起 —— 而且**不报错**,
//! 只是结果错。这些测试专门钉住对齐语义。

use std::time::Duration;

use lmflow::{Graph, Packet, Timestamp};

fn init() {
    lmflow::register_builtin_kernels();
}

fn zip_graph(policy: &str) -> Graph {
    init();
    let extra = if policy.is_empty() {
        String::new()
    } else {
        format!("    input_policy: {{ type: \"{policy}\" }}\n")
    };
    Graph::from_yaml(&format!(
        r#"
nodes:
  - name: "z"
    kernel: "ZipKernel"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
{extra}input_ports: ["x", "y"]
output_ports: ["out"]
"#
    ))
    .unwrap()
}

fn read_int(p: &Packet) -> i64 {
    p.as_i64().expect("should be an integer packet")
}

/// **核心用例**:两路速度不同,必须按时间戳配对,不能按到达顺序配对。
///
/// A 送 ts=0,1,2 三个;B 只送 ts=1 一个。
/// 正确行为:只有 ts=1 那一刻两路齐备;ts=0 时 B 尚未到达,故必须先等;
/// 一旦 B 的 ts=1 到达,ts=0 就确定「B 永远不会有 ts=0 的数据」,于是 ts=0 也可处理。
#[test]
fn pairs_by_timestamp_not_by_arrival_order() {
    let graph = zip_graph("");
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let a = graph.input("x").unwrap();
    let b = graph.input("y").unwrap();

    // A 先连送三个
    for i in 0..3i32 {
        a.send(Packet::from_i64((i * 10) as i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph.wait_until_idle().unwrap();
    assert!(
        poller.try_next().is_none(),
        "B never arrived, nothing should be produced at any timestamp -- otherwise it paired by arrival order"
    );

    // B 送 ts=1(跳过了 ts=0)
    b.send(Packet::from_i64(1_i64).at(Timestamp(1))).unwrap();
    graph.wait_until_idle().unwrap();

    // 现在 B 的边界已推到 2,故 ts=0 与 ts=1 都可判定
    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push((p.timestamp().0, read_int(&p)));
    }
    // ts=0:B 无数据 → ZipKernel 检测到缺一路,不产出
    // ts=1:A=10, B=1 → 产出 11
    assert_eq!(
        got,
        vec![(1, 11)],
        "only timestamps where both streams have data should produce, actual {got:?}"
    );

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// 两路都齐、时间戳一一对应:每个时刻都应正确配对。
#[test]
fn aligned_streams_pair_correctly() {
    let graph = zip_graph("");
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let a = graph.input("x").unwrap();
    let b = graph.input("y").unwrap();

    // 交错送,故意让到达顺序与时间戳顺序不一致
    for i in 0..5i32 {
        a.send(Packet::from_i64(i as i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    for i in 0..5i32 {
        b.send(Packet::from_i64((i * 100) as i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(10)).unwrap();

    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push((p.timestamp().0, read_int(&p)));
    }
    assert_eq!(
        got,
        (0..5).map(|i| (i, i + i * 100)).collect::<Vec<_>>(),
        "the two streams at the same timestamp must be paired together"
    );
}

/// 关流会把边界推到 Done,于是「另一路永远不会来」的时刻可以立刻结算。
#[test]
fn closing_one_input_unblocks_alignment() {
    let graph = zip_graph("");
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let a = graph.input("x").unwrap();

    for i in 0..3i32 {
        a.send(Packet::from_i64(i as i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph.wait_until_idle().unwrap();
    assert!(
        poller.try_next().is_none(),
        "B not closed and no data, should keep waiting"
    );

    // 关掉 B:等价于宣告「B 永远不会有数据」
    graph.close_input("y").unwrap();
    graph.wait_until_idle().unwrap();
    let st = graph.node_stats(0).unwrap();
    assert_eq!(
        st.processed, 3,
        "after B closes, all three of A's timestamps should be immediately processable (actual {} times)",
        st.processed
    );

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// `immediate` 策略**不做**对齐:任一路到达即触发。
#[test]
fn immediate_policy_skips_alignment() {
    let graph = zip_graph("immediate");
    graph.add_poller("out").unwrap();
    graph.start().unwrap();

    graph
        .input("x")
        .unwrap()
        .send(Packet::from_i64(1_i64).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();

    assert_eq!(
        graph.node_stats(0).unwrap().processed,
        1,
        "under immediate a single input triggers (sync would wait)"
    );

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// 丢包的算子不会卡住下游 —— 引擎会自动推进输出边界。
///
/// FilterKernel 把小于阈值的包丢掉。若不自动推进边界,下游会永远等那些时刻的数据。
#[test]
fn dropping_kernel_does_not_stall_downstream() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "f"
    kernel: "FilterKernel"
    input_ports: ["in"]
    output_ports: ["mid"]
    options: { threshold: 5 }
  - name: "z"
    kernel: "ZipKernel"
    input_ports: ["A:mid", "B:other"]
    output_ports: ["out"]
input_ports: ["in", "other"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let inp = graph.input("in").unwrap();
    let other = graph.input("other").unwrap();

    // 0..10 经过 Filter(阈值 5)后只剩 5..10;另一路每个时刻都送
    for i in 0..10i32 {
        inp.send(Packet::from_i64(i as i64).at(Timestamp(i as i64)))
            .unwrap();
        other
            .send(Packet::from_i64(1000_i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph
        .wait_done_timeout(Duration::from_secs(10))
        .expect("dropped timestamps must auto-advance the bound, otherwise this would time out");

    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(p.timestamp().0);
    }
    assert_eq!(
        got,
        (5..10).collect::<Vec<i64>>(),
        "only non-filtered timestamps should produce, and must not stall"
    );
}

/// 时间戳对齐在线程池下同样成立。
#[test]
fn alignment_holds_on_thread_pool() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }
nodes:
  - name: "z"
    kernel: "ZipKernel"
    executor: "cpu"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let a = graph.input("x").unwrap();
    let b = graph.input("y").unwrap();

    const N: i32 = 100;
    for i in 0..N {
        a.send(Packet::from_i64(i as i64).at(Timestamp(i as i64)))
            .unwrap();
        b.send(Packet::from_i64((i * 1000) as i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push((p.timestamp().0, read_int(&p)));
    }
    assert_eq!(got.len(), N as usize, "must not drop packets");
    for (ts, v) in &got {
        let i = *ts;
        assert_eq!(
            *v,
            i + i * 1000,
            "pairing at ts={ts} must be correct (no mispairing even under concurrency)"
        );
    }
}
