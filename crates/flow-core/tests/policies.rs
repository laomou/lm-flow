//! 输入策略测试(docs/design.md §7.10)。
//!
//! 三种策略的差别只在两处:**就绪条件**与**入队时是否丢包**。
//! 这里各自验证到,并特别确认 `fixed_size` 的丢包**绝不静默**。

use std::time::Duration;

use flow_core::{Graph, Packet, Timestamp};

fn init() {
    flow_core::register_builtin_kernels();
}

/// `sync`(默认):所有输入口齐备才触发。缺一路就不动。
#[test]
fn sync_waits_for_all_inputs() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "z"
    kernel: "ZipKernel"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    let int_id = flow_core::packet::fnv1a_type_id("i");
    // 只喂 A 口:不该产出
    graph
        .input("x")
        .unwrap()
        .send(Packet::new_interop(1i32, int_id).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();
    assert!(poller.try_next().is_none(), "缺一路输入时 sync 不该触发");

    // 补上 B 口:应产出 1+2=3
    graph
        .input("y")
        .unwrap()
        .send(Packet::new_interop(2i32, int_id).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();
    let out = poller.try_next().expect("齐备后应产出");
    let ptr = out.foreign_ptr().expect("应有数据");
    assert_eq!(unsafe { *(ptr as *const i32) }, 3);

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// `immediate`:任一口有数据就触发,不等其它口。
#[test]
fn immediate_fires_on_any_input() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "z"
    kernel: "ZipKernel"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
    input_policy: { type: "immediate" }
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();

    let int_id = flow_core::packet::fnv1a_type_id("i");
    // 只喂一路 —— immediate 下 Process 会被调用(ZipKernel 自己判断缺一路则不产出)
    graph
        .input("x")
        .unwrap()
        .send(Packet::new_interop(1i32, int_id).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();

    let st = graph.node_stats(0).unwrap();
    assert_eq!(
        st.processed, 1,
        "immediate 策略下单路数据也应触发 Process(sync 则不会)"
    );

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// `fixed_size`:队列满则丢最旧的包,且丢包必须可观测。
#[test]
fn fixed_size_drops_oldest_and_reports_it() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "p"
    kernel: "PassThroughKernel"
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: "fixed_size", capacity: 2 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    // 一次性灌 10 个但不驱动:容量 2,应丢掉 8 个最旧的
    for i in 0..10i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    assert_eq!(graph.queue_depth("in"), Some(2), "队列不得超过 capacity");
    assert_eq!(
        graph.dropped_count("in"),
        Some(8),
        "丢包数必须被记账(绝不静默)"
    );

    graph.wait_until_idle().unwrap();
    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(*p.get::<i32>().unwrap());
    }
    // 丢的是**最旧**的,留下的是最新两个
    assert_eq!(got, vec![8, 9], "应保留最新的包,丢弃最旧的");

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// `fixed_size` 与线程池并用:实时管线的典型配置。
#[test]
fn fixed_size_with_pool_bounds_memory() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - name: "p"
    kernel: "PassThroughKernel"
    executor: "cpu"
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: "fixed_size", capacity: 4 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    for i in 0..500i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    // 无论消费快慢,队列都不会超过 capacity —— 这正是它存在的意义
    assert!(
        graph.queue_depth("in").unwrap() <= 4,
        "fixed_size 必须把队列钉在 capacity 以内"
    );

    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
}

#[test]
fn fixed_size_rejects_zero_capacity() {
    init();
    // capacity 0 意味着每个包都丢,几乎肯定是漏配
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"],
      input_policy: { type: "fixed_size", capacity: 0 } }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("capacity"), "{err}");
}

#[test]
fn rejects_unknown_policy() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"],
      input_policy: { type: "nonsense" } }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("未知 input_policy"), "{err}");
}
