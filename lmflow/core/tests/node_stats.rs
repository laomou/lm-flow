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
