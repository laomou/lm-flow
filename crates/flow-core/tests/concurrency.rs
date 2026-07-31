//! 并发测试:线程池执行器。
//!
//! 这些测试是 M3 的验收标准。重点不是「能跑」,而是几件容易出错的事:
//!  * 节点真的跑在了**别的线程**上(不是静默退回主线程);
//!  * 阻塞接口在等线程池时不会**提前返回**,也不会**永久挂住**;
//!  * 并发下算子的 `Close` 只被调用一次(宿主线程与工作线程会同时想关它);
//!  * 所有权守恒在并发下仍然成立;
//!  * 混合执行器(一部分节点在池里、一部分在主线程)不会死锁。

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use flow_core::{Graph, Packet, Timestamp};

fn init() {
    flow_core::register_builtin_kernels();
}

/// 与 memory.rs 同样的所有权记账,但用于并发场景。
static ALIVE: AtomicIsize = AtomicIsize::new(0);
static ACCOUNTING: Mutex<()> = Mutex::new(());

unsafe extern "C" fn tracked_drop(p: *mut std::ffi::c_void) {
    ALIVE.fetch_sub(1, Ordering::SeqCst);
    drop(Box::from_raw(p as *mut i32));
}

fn tracked_packet(v: i32, ts: i64) -> Packet {
    ALIVE.fetch_add(1, Ordering::SeqCst);
    let ptr = Box::into_raw(Box::new(v)) as *mut std::ffi::c_void;
    unsafe { Packet::from_foreign(ptr, 0, Some(tracked_drop)) }.at(Timestamp(ts))
}

// ---------------------------------------------------------------- 基本正确性

#[test]
fn pool_declared_but_unused_is_still_valid() {
    init();
    // 定义了池但没节点用 —— 合法(会有 WARN),不该报错
    let g = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    assert_eq!(g.executor_names(), vec!["cpu"]);
}

#[test]
fn rejects_unnamed_and_duplicate_executors() {
    init();
    let err = Graph::from_yaml(
        r#"
executors:
  - { type: "ThreadPoolExecutor", num_threads: 2 }
nodes: []
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("name"), "{err}");

    let err = Graph::from_yaml(
        r#"
executors:
  - { name: "a", type: "ThreadPoolExecutor", num_threads: 1 }
  - { name: "a", type: "ThreadPoolExecutor", num_threads: 1 }
nodes: []
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("重复"), "{err}");
}

/// 全部节点跑在线程池上:结果必须完整且有序。
#[test]
fn all_nodes_on_pool_produce_correct_output() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }
nodes:
  - { name: "n1", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in"],  output_ports: ["m1"] }
  - { name: "n2", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["m1"], output_ports: ["m2"] }
  - { name: "n3", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["m2"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    const N: i32 = 200;
    for i in 0..N {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(*p.get::<i32>().unwrap());
    }
    assert_eq!(got.len(), N as usize, "不得丢包");
    // 单一线性链 + 每节点 max_in_flight=1,故顺序必须保持
    assert_eq!(got, (0..N).collect::<Vec<_>>(), "顺序必须保持");
}

/// 节点确实跑在**别的线程**上 —— 而不是静默退回主线程。
///
/// 用 Rust 的 observer 回调(**按图**注册)取代全局日志:回调在派发该包的线程上
/// 执行,于是能直接看到算子跑在哪个线程。按图隔离,不受并发跑的其它测试干扰。
#[test]
fn pool_nodes_actually_run_off_the_host_thread() {
    init();
    let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));

    {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - { name: "p", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
        )
        .unwrap();
        let rec = seen.clone();
        graph
            .observe("out", move |_pkt| {
                let name = std::thread::current()
                    .name()
                    .unwrap_or("(unnamed)")
                    .to_string();
                rec.lock().expect("锁中毒").push(name);
            })
            .unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..20i32 {
            input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        }
        graph.close_all_inputs();
        graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
    }

    let names = seen.lock().expect("锁中毒").clone();
    assert_eq!(names.len(), 20, "observer 应收到全部 20 个包");
    assert!(
        names.iter().all(|n| n.starts_with("cpu-")),
        "指定了 executor 的算子必须真的跑在 cpu-N 池线程上,实际: {names:?}"
    );
}

/// 混合执行器:一部分节点在池里、一部分在主线程 —— 最容易死锁的组合。
#[test]
fn mixed_executors_do_not_deadlock() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 3 }
nodes:
  - { name: "a", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in"], output_ports: ["m1"] }
  - { name: "b", kernel: "PassThroughKernel",                  input_ports: ["m1"], output_ports: ["m2"] }
  - { name: "c", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["m2"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    // 逐个「送一个取一个」—— 每次都要跨越 池→主线程→池 三段
    for i in 0..50i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        let p = poller
            .next_timeout(Duration::from_secs(10))
            .expect("不应超时")
            .expect("应有输出");
        assert_eq!(p.get::<i32>(), Some(&i));
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
}

/// 扇出到两条并行分支,再各自汇出 —— 检验并发分发。
#[test]
fn concurrent_fanout_branches() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }
nodes:
  - { name: "sp", kernel: "SplitKernel",       executor: "cpu", input_ports: ["in"], output_ports: ["a", "b"] }
  - { name: "pa", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["a"],  output_ports: ["oa"] }
  - { name: "pb", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["b"],  output_ports: ["ob"] }
input_ports: ["in"]
output_ports: ["oa", "ob"]
"#,
    )
    .unwrap();
    let pa = graph.add_poller("oa").unwrap();
    let pb = graph.add_poller("ob").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    const N: i32 = 100;
    for i in 0..N {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

    let count = |p: &flow_core::Poller| {
        let mut n = 0;
        while p.try_next().is_some() {
            n += 1;
        }
        n
    };
    assert_eq!(count(&pa), N as usize, "分支 a 应收到全部");
    assert_eq!(count(&pb), N as usize, "分支 b 应收到全部");
}

// ---------------------------------------------------------------- 阻塞语义

/// `wait_until_idle` 必须真的等到线程池干完,不能提前返回。
#[test]
fn wait_until_idle_waits_for_pool() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - { name: "p", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..100i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph
        .wait_until_idle_timeout(Duration::from_secs(30))
        .unwrap();
    assert!(graph.is_idle(), "idle 返回后必须真的空闲");

    let mut n = 0;
    while poller.try_next().is_some() {
        n += 1;
    }
    assert_eq!(n, 100, "idle 之后所有包都应已产出");
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(10)).unwrap();
}

/// 超时必须真的会超时(而不是永久阻塞或立即返回)。
#[test]
fn poller_timeout_actually_times_out() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 1 }
nodes:
  - { name: "p", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    // 一个包都不送:池空闲 → 应立刻判定「不会再有输出」而返回 Ok(None),不是挂住
    let t0 = std::time::Instant::now();
    let r = poller.next_timeout(Duration::from_millis(300));
    assert!(t0.elapsed() < Duration::from_secs(5), "不得永久挂住");
    assert!(
        matches!(r, Ok(None)),
        "空闲且无数据应返回 Ok(None),实际 {r:?}"
    );
}

/// 暂停后不再调度;恢复后暂停期间到达的包必须被处理(不能一直躺着)。
#[test]
fn pause_and_resume() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - { name: "p", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    graph.pause();
    assert!(graph.is_paused());
    let input = graph.input("in").unwrap();
    for i in 0..10i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    // 暂停期间不应产出
    std::thread::sleep(Duration::from_millis(100));
    assert!(poller.try_next().is_none(), "暂停期间不应有产出");
    assert_eq!(graph.queue_depth("in"), Some(10), "包应积压着");

    graph.resume();
    assert!(!graph.is_paused());
    graph
        .wait_until_idle_timeout(Duration::from_secs(30))
        .unwrap();
    let mut n = 0;
    while poller.try_next().is_some() {
        n += 1;
    }
    assert_eq!(n, 10, "恢复后暂停期间积压的包必须全部被处理");

    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(10)).unwrap();
}

// ---------------------------------------------------------------- 并发下的不变量

/// 算子的 `Close` 在并发下只能被调用一次 ——
/// 宿主线程的 `try_advance_closing` 与工作线程的 `finish` 会同时想关同一个节点。
///
/// 用**按图的计数器**断言,而不是全局日志回调:日志接收器是进程级的,并发跑的
/// 其它测试的算子日志会混进来,曾把这个测试搞成假失败。
#[test]
fn close_is_called_exactly_once_under_concurrency() {
    init();
    for round in 0..30 {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }
nodes:
  - { name: "s", kernel: "SinkKernel", executor: "cpu", input_ports: ["in"], output_ports: [] }
input_ports: ["in"]
"#,
        )
        .unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..30i32 {
            input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        }
        graph.close_all_inputs();
        graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

        assert_eq!(
            graph.counter_value("sink.closed"),
            1,
            "第 {round} 轮:Close 必须恰好一次"
        );
        assert_eq!(
            graph.counter_value("sink.packets"),
            30,
            "第 {round} 轮:每个包都应被处理一次"
        );
    }
}

/// 并发下所有权仍然守恒:每个外部 payload 的 drop_fn 恰好一次。
#[test]
fn ownership_conserved_under_concurrency() {
    let _lock = ACCOUNTING.lock().unwrap_or_else(|e| e.into_inner());
    let base = ALIVE.load(Ordering::SeqCst);
    init();
    {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }
nodes:
  - { name: "sp", kernel: "SplitKernel",       executor: "cpu", input_ports: ["in"], output_ports: ["a", "b"] }
  - { name: "pa", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["a"],  output_ports: ["oa"] }
  - { name: "pb", kernel: "SinkKernel",        executor: "cpu", input_ports: ["b"],  output_ports: [] }
input_ports: ["in"]
output_ports: ["oa"]
"#,
        )
        .unwrap();
        let poller = graph.add_poller("oa").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..300i32 {
            input.send(tracked_packet(i, i as i64)).unwrap();
        }
        graph.close_all_inputs();
        graph.wait_done_timeout(Duration::from_secs(60)).unwrap();
        while poller.try_next().is_some() {}
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "并发 + 扇出下仍不得泄漏或重复释放"
    );
}

/// 多个宿主线程同时送包(引擎必须是线程安全的)。
#[test]
fn multiple_host_threads_sending() {
    init();
    let graph = std::sync::Arc::new(
        Graph::from_yaml(
            r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }
nodes:
  - { name: "a", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in1"], output_ports: ["o1"] }
  - { name: "b", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in2"], output_ports: ["o2"] }
input_ports: ["in1", "in2"]
output_ports: ["o1", "o2"]
"#,
        )
        .unwrap(),
    );
    let p1 = graph.add_poller("o1").unwrap();
    let p2 = graph.add_poller("o2").unwrap();
    graph.start().unwrap();

    // 两个宿主线程各自往不同的图输入口送(单调性是按边独立记的)
    let g1 = graph.clone();
    let g2 = graph.clone();
    let t1 = std::thread::spawn(move || {
        let i = g1.input("in1").unwrap();
        for k in 0..100i32 {
            i.send(Packet::new(k).at(Timestamp(k as i64))).unwrap();
        }
    });
    let t2 = std::thread::spawn(move || {
        let i = g2.input("in2").unwrap();
        for k in 0..100i32 {
            i.send(Packet::new(k + 1000).at(Timestamp(k as i64)))
                .unwrap();
        }
    });
    t1.join().unwrap();
    t2.join().unwrap();

    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(60)).unwrap();

    let count = |p: &flow_core::Poller| {
        let mut n = 0;
        while p.try_next().is_some() {
            n += 1;
        }
        n
    };
    assert_eq!(count(&p1), 100);
    assert_eq!(count(&p2), 100);
}

/// 图在池仍有任务时被直接丢弃 —— 必须干净地关停并 join,不能崩也不能挂。
#[test]
fn dropping_graph_with_busy_pool_is_clean() {
    let _lock = ACCOUNTING.lock().unwrap_or_else(|e| e.into_inner());
    let base = ALIVE.load(Ordering::SeqCst);
    init();
    for _ in 0..10 {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }
nodes:
  - { name: "p", kernel: "PassThroughKernel", executor: "cpu", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
        )
        .unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..100i32 {
            input.send(tracked_packet(i, i as i64)).unwrap();
        }
        // 不等待,直接丢弃 —— 池里很可能还有任务在跑
        drop(input);
        drop(graph);
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "拆图时仍须释放所有在途 payload"
    );
}
