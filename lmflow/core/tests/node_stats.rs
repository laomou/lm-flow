//! 节点级运行统计(`NodeStats`,全原子无锁)+ DOT 热力图的验收 —— **纯 Rust,零 C++**。
//!
//! 钉住几件容易只做到一半的事:
//!   * `packets_in` / `packets_out` 真的按包累加(不是只在某条分支上);
//!   * `peak_queue_depth` 是**高水位**(排空后不回落);
//!   * `running` 靠 `in_flight > 0` 判断 —— `started_us` 归零时不清,故不能直接看它;
//!   * `to_dot_with_stats` 在标注统计的同时,**不破坏** subgraph cluster 与执行器图例。

use std::time::Duration;

use lmflow::{Graph, Packet, Timestamp};

const CHAIN: &str = r#"
nodes:
  - { name: a, kernel: PassThrough, input_ports: ["in"],  output_ports: ["mid"] }
  - { name: b, kernel: PassThrough, input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#;

fn run_chain(n: i64) -> Graph {
    let g = Graph::from_yaml(CHAIN).unwrap();
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
    let g = run_chain(4);
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
fn dot_with_stats_annotates_and_keeps_structure() {
    let g = run_chain(3);
    let plain = g.to_dot();
    let stats = g.to_dot_with_stats();

    // 统计标注只出现在 with_stats 版本
    assert!(!plain.contains("pkts"), "普通版不应带统计");
    assert!(stats.contains("3 pkts"), "应标出处理包数:\n{stats}");
    assert!(stats.contains("peakQ"), "应标出队列峰值");
    assert!(stats.contains("in 3 / out 3"), "应标出收发包数");

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

/// 子图 cluster 与统计模式共存(热力图不该吃掉 cluster)。
#[test]
fn dot_with_stats_keeps_subgraph_clusters() {
    let g = Graph::from_yaml(
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

/// `stats_timing: false` 关掉每次回调的两次 `Instant::now()`:
/// 耗时类字段归零、其余计数照常。这是**显式取舍**,不是 bug。
#[test]
fn stats_timing_off_zeroes_only_timing_fields() {
    let g = Graph::from_yaml(
        r#"
stats_timing: false
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
    assert_eq!(st.processed, 4, "计数不受计时开关影响");
    assert_eq!(st.packets_in, 4);
    assert_eq!(st.packets_out, 4);
    // 耗时归零
    assert_eq!(st.total_process_us, 0, "关了计时,累计耗时应为 0");
    assert_eq!(st.max_process_us, 0, "关了计时,最慢一次应为 0");
    assert_eq!(st.running_for_us, 0);

    // 热力图退化:全同色(不报错、不崩)
    let dot = g.to_dot_with_stats();
    assert!(dot.contains("4 pkts"), "包数仍应标出:\n{dot}");
}

/// `watchdog_ms > 0` 时,即使写了 `stats_timing: false` 也必须**强制开启**计时 ——
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

    let g = Graph::from_yaml(
        r#"
stats_timing: false
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
         若为 0 说明计时被 stats_timing=false 关掉了,watchdog 会静默失效",
        st.max_process_us
    );
}
