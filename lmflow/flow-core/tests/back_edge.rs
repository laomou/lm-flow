//! 反馈环(back-edge)端到端验收:最新值反馈寄存器语义。
//!
//! 关键断言:标记了 back_edges 的环能建、能跑、值正确,且**正向输入关闭后能正常终止**
//! (不再撞 wait_done 的 stuck 错误);未标记的环仍在建图期被拒。

use lmflow::{Graph, Packet, State, Timestamp};

fn init() {
    lmflow::register_builtin_kernels();
}

/// 自环运行累加:acc 的输出 out 经 back-edge 回灌到自己的 fb 口。
/// out(t) = in(t) + out(t-1)。喂 5 个 1 → [1,2,3,4,5];输入关闭后正常 Terminated。
#[test]
fn feedback_self_loop_accumulates_and_terminates() {
    init();
    // input_ports: [in(正向), out(反馈, back_edge)];output_ports: [out];out 自环回灌。
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: acc
    kernel: FeedbackAddKernel
    input_ports: ["in", "out"]
    output_ports: ["out"]
    back_edges: ["out"]
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .expect("a cycle broken by a back_edge must build");

    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    assert_eq!(graph.state(), State::Running);

    let input = graph.input("in").unwrap();
    for i in 0..5i64 {
        input.send(Packet::from_i64(1).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap(); // 若 back-edge 误触发 readiness,这里会挂死或撞 stuck 错误
    assert_eq!(graph.state(), State::Terminated);

    let got: Vec<i64> = std::iter::from_fn(|| poller.next().map(|p| p.as_i64().unwrap())).collect();
    assert_eq!(
        got,
        vec![1, 2, 3, 4, 5],
        "running sum via feedback register"
    );
}

/// 未标记 back_edges 的环(自环)仍在建图期被拒。
#[test]
fn unmarked_cycle_is_rejected() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: p, kernel: PassThroughKernel, input_ports: ["x"], output_ports: ["x"] }
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("cycle"), "{err}");
}

/// 节点的唯一输入口被标为 back_edge → 没有正向输入驱动,建图期拒。
#[test]
fn node_with_no_forward_input_is_rejected() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: p, kernel: PassThroughKernel, input_ports: ["x"], output_ports: ["x"], back_edges: ["x"] }
"#,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("forward input"),
        "should reject an all-back-edge node: {err}"
    );
}

/// back_edges 里写了不存在的输入口名 → 建图期拒(不静默)。
#[test]
fn back_edge_must_name_an_input_port() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - name: acc
    kernel: FeedbackAddKernel
    input_ports: ["in", "out"]
    output_ports: ["out"]
    back_edges: ["nope"]
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("not one of this node's input ports"),
        "{err}"
    );
}

/// 子图内部的反馈环:`back_edges` 名字随端口一并重映射,展开后自环仍成立并跑通。
#[test]
fn feedback_inside_subgraph_expands_and_runs() {
    init();
    let graph = Graph::from_yaml(
        r#"
subgraphs:
  Acc:
    nodes:
      - name: a
        kernel: FeedbackAddKernel
        input_ports: ["sin", "loop"]
        output_ports: ["loop"]
        back_edges: ["loop"]
    input_ports: ["sin"]
    output_ports: ["loop"]
nodes:
  - { name: acc, type: Acc, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .expect("subgraph with an internal back_edge must expand and build");

    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..4i64 {
        input.send(Packet::from_i64(2).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);

    let got: Vec<i64> = std::iter::from_fn(|| poller.next().map(|p| p.as_i64().unwrap())).collect();
    assert_eq!(
        got,
        vec![2, 4, 6, 8],
        "feedback loop survives subgraph expansion"
    );
}

/// 反馈自环跑在线程池上:worker 线程做 try_claim(back-edge 弹包)/ dispatch(cap-1),
/// 主线程 pump —— 并发触达被改动的调度热路径(TSan 覆盖)。max_in_flight=1 故仍确定。
#[test]
fn feedback_loop_on_thread_pool() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - name: acc
    kernel: FeedbackAddKernel
    executor: cpu
    input_ports: ["in", "out"]
    output_ports: ["out"]
    back_edges: ["out"]
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();

    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..6i64 {
        input.send(Packet::from_i64(1).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);

    let got: Vec<i64> = std::iter::from_fn(|| poller.next().map(|p| p.as_i64().unwrap())).collect();
    assert_eq!(got, vec![1, 2, 3, 4, 5, 6], "running sum on a thread pool");
}
