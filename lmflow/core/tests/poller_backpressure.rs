use lmflow::{Graph, InteropType, Packet, PollerOptions, PollerOverflow, Timestamp};

fn graph() -> Graph {
    Graph::from_yaml(
        r#"
nodes:
  - { name: pass, kernel: PassThrough, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap()
}

fn send(graph: &Graph, value: i64) {
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(value).at(Timestamp(value)))
        .unwrap();
    graph.wait_until_idle().unwrap();
}

fn finish(graph: &Graph) {
    graph.close_all_inputs();
    graph.wait_done().unwrap();
}

#[test]
fn poller_queue_is_included_in_global_watermarks() {
    let graph = graph();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    send(&graph, 7);
    let shared = graph.shared_for_inspection();
    assert_eq!(shared.total_queued(), 1);
    assert_eq!(shared.total_queued_bytes(), 8);

    assert_eq!(
        poller.try_next().and_then(|packet| packet.as_i64()),
        Some(7)
    );
    assert_eq!(shared.total_queued(), 0);
    assert_eq!(shared.total_queued_bytes(), 0);
    finish(&graph);
}

#[test]
fn each_poller_subscription_counts_as_an_independent_queue_slot() {
    let graph = graph();
    let first = graph.add_poller("out").unwrap();
    let second = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    send(&graph, 7);

    let shared = graph.shared_for_inspection();
    assert_eq!(shared.total_queued(), 2);
    assert_eq!(shared.total_queued_bytes(), 16);
    drop(first.try_next());
    assert_eq!(shared.total_queued(), 1);
    drop(second.try_next());
    assert_eq!(shared.total_queued(), 0);
    finish(&graph);
}

#[test]
fn poller_retention_triggers_graph_input_watermark() {
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: pass, kernel: PassThrough, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
max_queued_packets: 1
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    send(&graph, 0);

    let error = graph
        .input("in")
        .unwrap()
        .try_send(Packet::from_i64(1).at(Timestamp(1)))
        .unwrap_err();
    assert_eq!(error.code(), lmflow::status::code::WOULD_BLOCK);

    drop(poller.try_next());
    graph
        .input("in")
        .unwrap()
        .try_send(Packet::from_i64(1).at(Timestamp(1)))
        .unwrap();
    drop(poller.next());
    finish(&graph);
}

#[test]
fn registered_custom_payload_counts_toward_poller_bytes() {
    #[repr(C)]
    struct Point {
        x: i64,
        y: i64,
    }
    unsafe impl InteropType for Point {
        const TYPE_NAME: &'static str = "lmflow.test.PollerPoint";
    }

    let graph = graph();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_interop(Point { x: 1, y: 2 }).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();

    let shared = graph.shared_for_inspection();
    assert_eq!(shared.total_queued(), 1);
    assert_eq!(shared.total_queued_bytes(), 16);
    drop(poller.try_next());
    assert_eq!(shared.total_queued_bytes(), 0);
    finish(&graph);
}

#[test]
fn drop_oldest_retains_the_newest_capacity_packets() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options("out", PollerOptions::new(2, PollerOverflow::DropOldest))
        .unwrap();
    graph.start().unwrap();
    for value in 0..3 {
        send(&graph, value);
    }
    finish(&graph);

    let values: Vec<_> =
        std::iter::from_fn(|| poller.next().and_then(|packet| packet.as_i64())).collect();
    assert_eq!(values, vec![1, 2]);
    assert_eq!(poller.dropped_count(), 1);
}

#[test]
fn drop_newest_preserves_already_queued_packets() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options("out", PollerOptions::new(2, PollerOverflow::DropNewest))
        .unwrap();
    graph.start().unwrap();
    for value in 0..3 {
        send(&graph, value);
    }
    finish(&graph);

    let values: Vec<_> =
        std::iter::from_fn(|| poller.next().and_then(|packet| packet.as_i64())).collect();
    assert_eq!(values, vec![0, 1]);
    assert_eq!(poller.dropped_count(), 1);
}

#[test]
fn latest_retains_only_the_most_recent_packet() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options("out", PollerOptions::new(1, PollerOverflow::Latest))
        .unwrap();
    graph.start().unwrap();
    for value in 0..4 {
        send(&graph, value);
    }
    finish(&graph);

    assert_eq!(poller.next().and_then(|packet| packet.as_i64()), Some(3));
    assert!(poller.next().is_none());
    assert_eq!(poller.dropped_count(), 3);
}

#[test]
fn block_waits_until_the_host_drains_a_slot() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options("out", PollerOptions::new(1, PollerOverflow::Block))
        .unwrap();
    graph.start().unwrap();
    send(&graph, 0);

    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            graph
                .input("in")
                .unwrap()
                .send(Packet::from_i64(1).at(Timestamp(1)))
                .unwrap();
            graph.wait_until_idle().unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            poller.try_next().and_then(|packet| packet.as_i64()),
            Some(0)
        );
        worker.join().unwrap();
    });

    assert_eq!(
        poller.try_next().and_then(|packet| packet.as_i64()),
        Some(1)
    );
    finish(&graph);
}

#[test]
fn reset_clears_accounted_poller_packets_and_drop_count() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options("out", PollerOptions::new(1, PollerOverflow::DropOldest))
        .unwrap();
    graph.start().unwrap();
    send(&graph, 0);
    send(&graph, 1);
    finish(&graph);
    assert_eq!(graph.shared_for_inspection().total_queued(), 1);
    assert_eq!(poller.dropped_count(), 1);

    graph.reset().unwrap();
    assert_eq!(graph.shared_for_inspection().total_queued(), 0);
    assert_eq!(poller.dropped_count(), 0);
    assert!(poller.try_next().is_none());
}

#[test]
fn invalid_bounded_options_are_rejected() {
    let graph = graph();
    assert!(graph
        .add_poller_with_options("out", PollerOptions::new(0, PollerOverflow::DropOldest))
        .is_err());
    assert!(graph
        .add_poller_with_options("out", PollerOptions::new(2, PollerOverflow::Latest))
        .is_err());
}

#[test]
fn dropping_poller_releases_accounting_and_unblocks_producer() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options("out", PollerOptions::new(1, PollerOverflow::Block))
        .unwrap();
    graph.start().unwrap();
    send(&graph, 0);
    assert_eq!(graph.shared_for_inspection().total_queued(), 1);

    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            graph
                .input("in")
                .unwrap()
                .send(Packet::from_i64(1).at(Timestamp(1)))
                .unwrap();
            graph.wait_until_idle().unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(poller);
        worker.join().unwrap();
    });

    assert_eq!(graph.shared_for_inspection().total_queued(), 0);
    finish(&graph);
}

/// **回归:顺序式宿主写法不得永久挂死。**
///
/// 仓库里其它测试与示例用的都是这种写法:先把包送完、`close_all_inputs()`、`wait_done()`,
/// **最后**才排 poller。而 `Block` 是在 `dispatch` 内部原地等宿主腾位的 —— 主线程执行器下
/// 派发就跑在宿主自己的线程上,于是宿主既是生产者又是唯一消费者:它卡在 `push` 里,
/// 永远走不到 `poller.next()`,`wait_done()` 也就永不返回。
///
/// 修复前实测:capacity=2、送 5 个包 → `wait_done()` **永久挂死**(6s 看门狗超时)。
/// 现在应当在 `block_timeout` 到点后记录图错误并让 `wait_done()` 返回 Err,
/// 且报错要说清前置条件与替代方案。
#[test]
fn block_does_not_hang_a_sequential_host() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options(
            "out",
            PollerOptions::new(2, PollerOverflow::Block)
                .with_block_timeout(Some(std::time::Duration::from_millis(300))),
        )
        .unwrap();
    graph.start().unwrap();

    // 容量 2,却要送 5 个 —— 必然撞满,而宿主此刻不可能排水。
    for value in 0..5i64 {
        let _ = graph
            .input("in")
            .unwrap()
            .send(Packet::from_i64(value).at(Timestamp(value)));
    }
    graph.close_all_inputs();

    // 关键:必须**返回**(不论 Ok/Err),不能挂住。给足余量,真挂死会在这里超时。
    let done = graph.wait_done_timeout(std::time::Duration::from_secs(20));
    let err = done.expect_err("Block 撞上顺序式宿主应报错,而不是静默成功");
    let msg = err.to_string();
    assert!(
        msg.contains("Block") && msg.contains("concurrently"),
        "报错必须点明 Block 需要并发排水的宿主: {msg}"
    );
    assert!(
        msg.contains("Latest") || msg.contains("DropOldest"),
        "报错必须给出替代方案: {msg}"
    );
    // 已入队的那些仍可取走 —— 失败的是「等不到位」,不是把队列弄坏了。
    assert!(poller.try_next().is_some(), "已入队的包应仍可取出");
}

/// 无上界(`with_block_timeout(None)`)保留原语义:确有并发排水者时一直等。
/// 这条与 `block_waits_until_the_host_drains_a_slot` 配对,证明上界是**可选**的,
/// 不是把 Block 改成了有损策略。
#[test]
fn block_without_timeout_still_waits_for_a_concurrent_drainer() {
    let graph = graph();
    let poller = graph
        .add_poller_with_options(
            "out",
            PollerOptions::new(1, PollerOverflow::Block).with_block_timeout(None),
        )
        .unwrap();
    graph.start().unwrap();
    send(&graph, 0);

    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            graph
                .input("in")
                .unwrap()
                .send(Packet::from_i64(1).at(Timestamp(1)))
                .unwrap();
            graph.wait_until_idle().unwrap();
        });
        // 比任何默认上界都久 —— 若上界仍在生效,这里就会失败。
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert_eq!(
            poller.try_next().and_then(|packet| packet.as_i64()),
            Some(0)
        );
        worker.join().unwrap();
    });

    assert_eq!(
        poller.try_next().and_then(|packet| packet.as_i64()),
        Some(1)
    );
    finish(&graph);
}
