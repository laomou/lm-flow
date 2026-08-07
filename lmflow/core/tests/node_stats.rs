//! 节点级运行统计(`NodeStats`,全原子无锁)+ DOT 热力图的验收 —— **纯 Rust,零 C++**。
//!
//! 钉住几件容易只做到一半的事:
//!   * `packets_in` / `packets_out` 真的按包累加(不是只在某条分支上);
//!   * `peak_queue_depth` 是**高水位**(排空后不回落);
//!   * `running` 靠 `in_flight > 0` 判断 —— `started_us` 归零时不清,故不能直接看它;
//!   * `to_dot_with_stats` 在标注统计的同时,**不破坏** subgraph cluster 与执行器图例。

mod common;

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use lmflow::{DotView, Graph, Kernel, KernelCtx, Packet, Timestamp};

static RUNNING_STATE_GATE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static EXECUTOR_QUEUE_GATE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

struct RunningGateGuard;

impl RunningGateGuard {
    fn hold() -> Self {
        *RUNNING_STATE_GATE.0.lock().unwrap() = false;
        Self
    }
}

impl Drop for RunningGateGuard {
    fn drop(&mut self) {
        *RUNNING_STATE_GATE.0.lock().unwrap() = true;
        RUNNING_STATE_GATE.1.notify_all();
    }
}

struct ExecutorQueueGateGuard;

impl ExecutorQueueGateGuard {
    fn hold() -> Self {
        *EXECUTOR_QUEUE_GATE.0.lock().unwrap() = false;
        Self
    }
}

impl Drop for ExecutorQueueGateGuard {
    fn drop(&mut self) {
        *EXECUTOR_QUEUE_GATE.0.lock().unwrap() = true;
        EXECUTOR_QUEUE_GATE.1.notify_all();
    }
}

#[derive(Default)]
struct DotRunning;

impl Kernel for DotRunning {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        let (lock, wake) = &RUNNING_STATE_GATE;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        let _ = context;
        Ok(())
    }
}

#[derive(Default)]
struct DotError;

impl Kernel for DotError {
    fn process(&mut self, _context: &mut KernelCtx) -> lmflow::Result<()> {
        Err(lmflow::Error::Kernel("intentional DOT state error".into()))
    }
}

#[derive(Default)]
struct DotQueued;

impl Kernel for DotQueued {
    fn process(&mut self, _context: &mut KernelCtx) -> lmflow::Result<()> {
        let (lock, wake) = &EXECUTOR_QUEUE_GATE;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        Ok(())
    }
}

const CHAIN: &str = r#"
nodes:
  - { name: a, kernel: PassThrough, input_ports: ["in"],  output_ports: ["mid"] }
  - { name: b, kernel: PassThrough, input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#;

/// 同一条链,但显式挂**委托执行器**(交还宿主线程)。
///
/// 只给那些断言**精确队列深度**的用例用:队列高水位本质上是调度产物 ——
/// 默认执行器是线程池,「送一个」与「worker 取一个」并发进行,峰值可能是 1 也可能是 2。
/// 交还宿主线程后执行严格同步,峰值恒为 1,那种精确断言才有意义。
const CHAIN_SYNC: &str = r#"
stats: full
executors:
  - { name: "host", type: "DelegatingExecutor" }
nodes:
  - { name: a, kernel: PassThrough, executor: "host", input_ports: ["in"],  output_ports: ["mid"] }
  - { name: b, kernel: PassThrough, executor: "host", input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#;

fn run_chain(n: i64) -> Graph {
    run_chain_of(CHAIN, n)
}

fn run_chain_of(yaml: &str, n: i64) -> Graph {
    let g = common::graph_from_yaml(yaml).unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in 0..n {
        inp.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
    }
    g.close_all_inputs();
    let mut got = 0;
    while got < n {
        match out.next() {
            Some(_) => got += 1,
            None => break,
        }
    }
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(got, n, "应收到全部输出");
    g
}

#[derive(Default)]
struct ThreadPoolCowMutator;

impl Kernel for ThreadPoolCowMutator {
    fn get_contract(contract: &mut lmflow::KernelContract) {
        contract.input_any(0);
        contract.output_any(0);
    }

    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        let mut packet = context.take_input(0);
        packet.make_mutable_builtin()?;
        context.emit(0, packet)
    }
}

#[test]
fn thread_pool_linear_cow_releases_upstream_references_before_wakeup() {
    lmflow::register_kernel::<ThreadPoolCowMutator>("ThreadPoolCowMutator").unwrap();
    let graph = common::graph_from_yaml(
        r#"
stats: full
executors:
  - { name: pool, type: ThreadPoolExecutor, num_threads: 4 }
nodes:
  - { name: pass, kernel: PassThrough, executor: pool, input_ports: [in], output_ports: [mid] }
  - { name: mutate, kernel: ThreadPoolCowMutator, executor: pool, input_ports: [mid], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for ts in 0..128 {
        input.send(Packet::from_i64(ts).at(Timestamp(ts))).unwrap();
    }
    graph.close_all_inputs();
    let mut received = 0;
    while received < 128 {
        if poller.next().is_none() {
            break;
        }
        received += 1;
    }
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(received, 128);
    let dot = graph.to_dot_with_stats();
    assert!(
        dot.contains("mutate") && dot.contains("CoW 0 copies"),
        "linear thread-pool pipeline copied despite exclusive ownership:\n{dot}"
    );
}

#[test]
fn counts_packets_in_and_out() {
    let g = run_chain(6);
    for i in 0..2 {
        let st = g.node_stats(i).expect("node stats");
        assert_eq!(st.processed, 6, "{} 应处理 6 次", st.node_name);
        assert_eq!(st.packets_in, 6, "{} 收 6 个包", st.node_name);
        assert_eq!(st.packets_out, 6, "{} 发 6 个包", st.node_name);
        assert_eq!(st.errors, 0);
        // 跑完了,不该还在跑
        assert!(!st.running, "{} 已跑完", st.node_name);
        assert_eq!(st.running_for_us, 0, "不在跑时 running_for_us 应为 0");
        assert_eq!(st.queued, 0, "队列应排空");
    }
}

#[test]
fn peak_queue_depth_is_a_high_water_mark() {
    let g = run_chain(6);
    // 队列已排空(queued == 0),但峰值必须留着 —— 它是高水位,不回落。
    let b = g.node_stats(1).expect("node stats");
    assert_eq!(b.queued, 0);
    assert!(
        b.peak_queue_depth >= 1,
        "峰值应至少为 1(排空后仍保留),实际 {}",
        b.peak_queue_depth
    );
}

#[test]
fn total_us_is_consistent_with_processed() {
    let g = run_chain_of(&format!("stats: full\n{CHAIN}"), 4);
    let st = g.node_stats(0).unwrap();
    assert!(
        st.total_process_us >= 0 && st.max_process_us >= 0,
        "耗时不应为负"
    );
    assert!(
        st.max_process_us <= st.total_process_us,
        "单次最慢不可能超过累计:max={} total={}",
        st.max_process_us,
        st.total_process_us
    );
}

#[test]
fn diagnostics_show_latency_percentiles() {
    #[derive(Default)]
    struct PercentileSlow;
    impl Kernel for PercentileSlow {
        fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
            std::thread::sleep(Duration::from_millis(2));
            context.forward(0, 0)
        }
    }
    let _ = lmflow::register_kernel::<PercentileSlow>("PercentileSlow");
    let graph = common::graph_from_yaml(
        r#"
stats: full
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: slow, kernel: PercentileSlow, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    for timestamp in 0..3 {
        graph
            .input("in")
            .unwrap()
            .send(Packet::from_i64(timestamp).at(Timestamp(timestamp)))
            .unwrap();
        output.next().expect("slow node should emit");
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(2)).unwrap();

    let diagnostics = graph.to_dot_with_view(DotView::Diagnostics);
    assert!(
        diagnostics.contains("PercentileSlow · Rust"),
        "{diagnostics}"
    );
    assert!(diagnostics.contains("\\nlat p50 "), "{diagnostics}");
    assert!(diagnostics.contains(" · p95 "), "{diagnostics}");
    assert!(diagnostics.contains(" · p99 "), "{diagnostics}");
    assert!(
        !graph.to_dot_compact().contains("\\nlat p50 "),
        "percentiles should stay in the diagnostics view"
    );
}

#[test]
fn dot_with_stats_annotates_and_keeps_structure() {
    // 精确断言 peakQ 需要同步执行 —— 见 CHAIN_SYNC 的说明。
    let g = run_chain_of(CHAIN_SYNC, 3);
    let plain = g.to_dot();
    let stats = g.to_dot_with_stats();

    // 统计标注只出现在 with_stats 版本
    assert!(!plain.contains("pkts"), "普通版不应带统计");
    assert!(stats.contains("3 pkts"), "应标出处理包数:\n{stats}");
    assert!(stats.contains("peakQ"), "应标出队列峰值");
    assert!(
        stats.contains("queued 0 · running 0/"),
        "应标出执行器当前队列/运行数"
    );
    assert!(stats.contains("· peak "), "应标出执行器峰值队列");
    assert!(stats.contains("· done "), "应标出执行器完成任务数");
    assert!(stats.contains("peakQ 1 / 8B"), "应标出队列字节峰值");
    assert!(
        stats.contains("in 3 (+3) / out 3 (+3)"),
        "应标出累计与区间收发包数"
    );
    assert!(stats.contains("e2e p50 "), "应标出端到端延迟");
    assert!(stats.contains("frames 3"), "端到端帧数应与输出一致");
    assert!(!plain.contains("e2e p50 "), "拓扑图不应记录端到端延迟");
    assert!(stats.contains("ports:"), "节点内应包含端口摘要");
    assert!(stats.contains("snapshot +"), "标题应标出本轮快照时长");
    assert!(
        stats.contains("window since start"),
        "首次统计图应使用本轮启动作为区间基线"
    );
    assert!(
        stats.contains("cluster_diagnostics_legend"),
        "统计图应包含诊断图例"
    );
    assert!(
        stats.contains("producer currently stalled"),
        "图例应解释 BLOCKED"
    );
    assert!(
        stats.contains("likely missing aligned input"),
        "图例应解释 WAITING"
    );
    assert!(stats.contains("tooltip="), "SVG 应包含悬停详情");
    assert!(
        !stats.contains("queue 0/unbounded · reserved 0"),
        "正常边应折叠详细统计"
    );

    // 结构不被破坏:两版节点数、边数一致,且都是合法 digraph
    assert!(stats.starts_with("digraph"));
    let count = |s: &str, pat: &str| s.matches(pat).count();
    assert_eq!(
        count(&plain, "->"),
        count(&stats, "->"),
        "边数不应因统计模式改变"
    );
    assert_eq!(
        count(&plain, "[label="),
        count(&stats, "[label="),
        "节点数不应因统计模式改变"
    );
}

#[test]
fn dot_marks_saturated_executor_and_lists_queued_nodes() {
    let _ = lmflow::register_kernel::<DotQueued>("DotQueued");
    let gate = ExecutorQueueGateGuard::hold();
    let graph = common::graph_from_yaml(
        r#"
stats: full
executors:
  - { name: solo, num_threads: 1 }
nodes:
  - { name: busy, kernel: DotQueued, executor: solo, input_ports: [busy_in], output_ports: [] }
  - { name: queued_a, kernel: DotQueued, executor: solo, input_ports: [a_in], output_ports: [] }
  - { name: queued_b, kernel: DotQueued, executor: solo, input_ports: [b_in], output_ports: [] }
input_ports: [busy_in, a_in, b_in]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("busy_in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();

    let running_deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let dot = graph.to_dot_with_stats();
        if dot.contains("queued 0 · running 1/1") {
            break;
        }
        assert!(
            std::time::Instant::now() < running_deadline,
            "busy node never occupied the single executor thread"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    graph
        .input("a_in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    graph
        .input("b_in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let dot = graph.to_dot_with_stats();
        if dot.contains("queued 2 · running 1/1") {
            assert!(dot.contains("fillcolor=\"#ffe4b5\""), "{dot}");
            assert!(dot.contains("queue: queued_a (1), queued_b (1)"), "{dot}");
            assert!(dot.contains("wait "), "{dot}");
            assert!(dot.contains("exec "), "{dot}");
            assert!(dot.contains("legend_executor_hot"), "{dot}");
            let sustained_deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let sustained = graph.to_dot_with_stats();
                if sustained.contains("fillcolor=\"#ffd6d6\"") {
                    break;
                }
                assert!(
                    std::time::Instant::now() < sustained_deadline,
                    "持续排队超过 1 秒应标红:\n{sustained}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "executor never reached saturated snapshot"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    drop(gate);
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn dot_stats_use_deltas_between_exports() {
    let graph = run_chain(2);
    let first = graph.to_dot_with_stats();
    assert!(first.contains("2 pkts (+2 ·"));

    std::thread::sleep(Duration::from_millis(2));
    let second = graph.to_dot_with_stats();
    assert!(second.contains("window "));
    assert!(!second.contains("window since start"));
    assert!(second.contains("2 pkts (+0 · 0"));
    assert!(second.contains("in 2 (+0) / out 2 (+0)"));
}

#[test]
fn start_rebases_dot_interval_after_prestart_export() {
    let graph = common::graph_from_yaml(CHAIN).unwrap();
    let prestart = graph.to_dot_compact();
    assert!(prestart.contains("window since start 0µs"));

    graph.start().unwrap();
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(2)).unwrap();
    let running = graph.to_dot_compact();
    assert!(running.contains("window since start"));
}

#[test]
fn dot_view_modes_separate_compact_and_diagnostics() {
    let graph = common::graph_from_yaml(CHAIN).unwrap();
    let compact = graph.to_dot_compact();
    let explicit = graph.to_dot_with_view(DotView::Compact);
    let diagnostics = graph.to_dot_with_stats();

    assert!(compact.contains("@default\\nCREATED"));
    assert!(explicit.contains("@default\\nCREATED"));
    assert!(compact.contains("window since start"));
    assert!(explicit.contains("window "));
    assert!(!compact.contains("CREATED · 0 pkts"));
    assert!(compact.contains("cluster_node_state_legend"));
    assert!(!compact.contains("ports:"));
    assert!(!compact.contains("cluster_diagnostics_legend"));
    assert!(diagnostics.contains("ports:"));
    assert!(diagnostics.contains("cluster_diagnostics_legend"));
}

#[test]
fn dot_node_state_tracks_idle_running_closed_and_error() {
    let _ = lmflow::register_kernel::<DotRunning>("DotRunning");
    let _ = lmflow::register_kernel::<DotError>("DotError");

    let idle = common::graph_from_yaml(
        r#"
nodes:
  - { name: idle, kernel: Sink, input_ports: [in], output_ports: [] }
input_ports: [in]
"#,
    )
    .unwrap();
    idle.start().unwrap();
    let idle_dot = idle.to_dot_compact();
    assert!(idle_dot.contains("@default\\nIDLE"));
    assert!(idle_dot.contains("hotspots running 0 · error 0"));
    assert!(idle_dot.contains("color=\"#4c78a8\""));
    idle.close_all_inputs();
    idle.wait_done_timeout(Duration::from_secs(2)).unwrap();
    let closed_dot = idle.to_dot_compact();
    assert!(closed_dot.contains("@default\\nCLOSED"));

    let running = common::graph_from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: running, kernel: DotRunning, input_ports: [in], output_ports: [], executor: pool }
input_ports: [in]
"#,
    )
    .unwrap();
    running.start().unwrap();
    let gate = RunningGateGuard::hold();
    running
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let dot = running.to_dot_compact();
        if dot.contains("@pool\\nRUNNING") {
            assert!(dot.contains("color=\"#2ca02c\", penwidth=3"));
            assert!(dot.contains("hotspots running 1 · error 0"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "node never entered RUNNING state"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(gate);
    running.close_all_inputs();
    running.wait_done_timeout(Duration::from_secs(2)).unwrap();

    let failed = common::graph_from_yaml(
        r#"
nodes:
  - { name: failed, kernel: DotError, input_ports: [in], output_ports: [] }
input_ports: [in]
"#,
    )
    .unwrap();
    failed.start().unwrap();
    failed
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    failed.close_all_inputs();
    let error = failed
        .wait_done_timeout(Duration::from_secs(2))
        .unwrap_err();
    assert!(error.to_string().contains("intentional DOT state error"));
    let error_dot = failed.to_dot_compact();
    assert!(error_dot.contains("@default"));
    assert!(error_dot.contains("\\nERROR"));
    assert!(error_dot.contains("hotspots running 0 · error 1"));
    assert!(error_dot.contains("color=\"#d62728\", penwidth=3"));
}

#[test]
fn dot_truncates_long_labels_but_keeps_full_tooltips_and_layout_hints() {
    let graph = common::graph_from_yaml(
        r#"
executors:
  - { name: extremely_long_executor_name_for_layout_grouping, num_threads: 1 }
nodes:
  - name: extremely_long_namespace_name_for_visualization/extremely_long_node_name_for_visualization
    kernel: PassThrough
    input_ports: [extremely_long_input_port_name_for_visualization]
    output_ports: [extremely_long_output_port_name_for_visualization]
    executor: extremely_long_executor_name_for_layout_grouping
input_ports: [extremely_long_input_port_name_for_visualization]
output_ports: [extremely_long_output_port_name_for_visualization]
"#,
    )
    .unwrap();

    let dot = graph.to_dot_with_stats();
    assert!(dot.contains("extremely_long_node_"));
    assert!(dot.contains('…'));
    assert!(!dot.contains("label=\"extremely_long_node_name_for_visualization"));
    assert!(dot.contains(
        "tooltip=\"extremely_long_namespace_name_for_visualization/extremely_long_node_name_for_visualization"
    ));
    assert!(dot.contains("extremely_long_input_port_na…"));
    assert!(dot.contains("graph input extremely_long_input_port_name_for_visualization"));
    assert!(
        dot.contains("tooltip=\"graph output extremely_long_output_port_name_for_visualization\"")
    );
    assert!(dot.contains("group=\"exec1\""));
    assert!(dot.contains("newrank=true"));
    assert!(dot.contains("nodesep=0.35, ranksep=0.65"));
    assert!(dot.contains("ordering=out"));
}

/// 子图 cluster 与统计模式共存(热力图不该吃掉 cluster)。
#[test]
fn dot_with_stats_keeps_subgraph_clusters() {
    let g = common::graph_from_yaml(
        r#"
subgraphs:
  inner:
    nodes:
      - { name: p, kernel: PassThrough, input_ports: ["sin"], output_ports: ["sout"] }
    input_ports: ["sin"]
    output_ports: ["sout"]
nodes:
  - { name: sub, type: inner, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let dot = g.to_dot_with_stats();
    assert!(dot.contains("cluster_"), "子图 cluster 应保留:\n{dot}");
}

/// 默认 `basic` 关闭每次回调计时，但保留低成本计数。
#[test]
fn basic_stats_zeroes_only_full_fields() {
    let g = common::graph_from_yaml(
        r#"
stats: basic
nodes:
  - { name: a, kernel: PassThrough, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in 0..4i64 {
        inp.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
    }
    g.close_all_inputs();
    let mut n = 0;
    while n < 4 {
        match out.next() {
            Some(_) => n += 1,
            None => break,
        }
    }
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();

    let st = g.node_stats(0).unwrap();
    // 计数照常
    assert_eq!(st.processed, 4, "basic 应保留吞吐计数");
    assert_eq!(st.packets_in, 4);
    assert_eq!(st.packets_out, 4);
    // 耗时归零
    assert_eq!(st.total_process_us, 0, "关了计时,累计耗时应为 0");
    assert_eq!(st.max_process_us, 0, "关了计时,最慢一次应为 0");
    assert_eq!(st.running_for_us, 0);

    // 热力图退化:全同色(不报错、不崩)
    let dot = g.to_dot_with_stats();
    assert!(dot.contains("4 pkts"), "包数仍应标出:\n{dot}");
    assert!(
        dot.contains("timing n/a"),
        "basic 应明确标出未采集耗时:\n{dot}"
    );
    assert!(dot.contains("detailed stats n/a"));
}

/// `watchdog_ms > 0` 时,即使写了 `stats: off` 也必须**强制开启** full ——
/// 否则 watchdog 无从判断超时、会静默失效。
///
/// 用一个**故意睡 2ms** 的算子做决定性判据:强制开启则 `max_process_us >= 1000`;
/// 若真被关掉,它会恒为 0。(PassThrough 快于 1µs、`as_micros()` 本就是 0,证明不了。)
#[test]
fn watchdog_forces_timing_on() {
    #[derive(Default)]
    struct Slow;
    impl lmflow::Kernel for Slow {
        fn process(&mut self, cc: &mut lmflow::KernelCtx) -> lmflow::Result<()> {
            std::thread::sleep(Duration::from_millis(2));
            cc.forward(0, 0)
        }
    }
    lmflow::register_kernel::<Slow>("SlowForWatchdogTest").unwrap();

    let g = common::graph_from_yaml(
        r#"
stats: off
watchdog_ms: 1
nodes:
  - { name: a, kernel: SlowForWatchdogTest, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    g.input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    g.close_all_inputs();
    let _ = out.next();
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();

    let st = g.node_stats(0).unwrap();
    assert_eq!(st.processed, 1);
    assert!(
        st.max_process_us >= 1000,
        "watchdog_ms>0 必须强制开启计时(睡了 2ms,应测到 >=1000µs);实测 {}µs —— \
         若为 0 说明 full 统计未被强制开启,watchdog 会静默失效",
        st.max_process_us
    );
}

#[test]
fn stats_off_keeps_state_and_errors_but_skips_throughput_counters() {
    let g = common::graph_from_yaml(
        r#"
stats: off
nodes:
  - { name: a, kernel: PassThrough, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    g.input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    g.close_all_inputs();
    assert!(out.next().is_some());
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();

    let st = g.node_stats(0).unwrap();
    assert!(!st.running);
    assert_eq!(st.queued, 0);
    assert_eq!(st.errors, 0);
    assert_eq!(st.processed, 0);
    assert_eq!(st.packets_in, 0);
    assert_eq!(st.packets_out, 0);
    assert_eq!(st.peak_queue_depth, 0);
    assert_eq!(st.total_process_us, 0);
    let dot = g.to_dot_with_stats();
    assert!(dot.contains("stats off"));
    assert!(!dot.contains("1 pkts"), "off 不应把未采集吞吐画成真实 0/1");
}
