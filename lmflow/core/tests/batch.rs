//! 批处理(`input_policy: batch`)端到端验收。
//!
//! 关键断言:攒够 N 个包一次交给算子(`process()` 用 `input_count`/`input_at` 读整批);
//! 关流时不足一批也刷出(不丢数据);正常终止。

#![cfg(feature = "builtin-kernels")] // 用内置 C++ 算子:纯 Rust 构建(--no-default-features)时整文件跳过

use lmflow::{Graph, Packet, State, Timestamp};

fn init() {
    lmflow::register_builtin_kernels();
}

const BATCH3: &str = r#"
nodes:
  - name: b
    kernel: BatchSumKernel
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: batch, capacity: 3 }
input_ports: ["in"]
output_ports: ["out"]
"#;

/// cap=3 喂 1..=10 → 批 [1,2,3]=6、[4,5,6]=15、[7,8,9]=24,关流刷余批 [10]=10。
#[test]
fn batch_sums_and_flushes_partial_on_close() {
    init();
    let graph = Graph::from_yaml(BATCH3).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 1..=10i64 {
        input
            .send(Packet::from_i64(i).at(Timestamp(i - 1)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);

    let got: Vec<i64> = std::iter::from_fn(|| poller.next().map(|p| p.as_i64().unwrap())).collect();
    assert_eq!(
        got,
        vec![6, 15, 24, 10],
        "three full batches then the partial batch flushed on close"
    );
}

/// 批处理跑在线程池上:worker 线程做 try_claim 批弹包,主线程 pump —— 并发触达调度热路径(TSan)。
#[test]
fn batch_on_thread_pool() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - name: b
    kernel: BatchSumKernel
    executor: cpu
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: batch, capacity: 4 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..8i64 {
        input.send(Packet::from_i64(1).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);
    let got: Vec<i64> = std::iter::from_fn(|| poller.next().map(|p| p.as_i64().unwrap())).collect();
    assert_eq!(got, vec![4, 4], "two full batches of four 1s");
}

/// batch 的 capacity 必须 >= 1(0 意味着「永远攒不满」,几乎肯定是漏配)。
/// 多输入口**已支持**(按时间戳对齐,见 `tests/batch_multi.rs`),故不再拒。
#[test]
fn batch_rejects_zero_capacity() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - name: b
    kernel: BatchSumKernel
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: batch, capacity: 0 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("batch size"), "{err}");
}

/// 多输入口的 batch 现在建图**通过**(此前被拒)。语义验收在 `tests/batch_multi.rs`。
#[test]
fn batch_accepts_multi_input() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: b
    kernel: ZipKernel
    input_ports: ["x", "y"]
    output_ports: ["out"]
    input_policy: { type: batch, capacity: 2 }
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    );
    assert!(graph.is_ok(), "多输入口 batch 应可建图:{:?}", graph.err());
}
