//! 子图(subgraph)+ 跨文件 `include` 的端到端验收:展开成扁平图后真实建图、真实跑通。
//!
//! 核心断言:子图展开出的图与手写扁平图**行为等价**(同样的直通链),且运行时引擎
//! 完全不感知子图 —— 展开是纯建图期变换。

use lmflow::{Graph, Packet, State, Timestamp};

fn init() {
    lmflow::register_builtin_kernels();
}

/// 把一张直通图跑 10 个包,校验值与时间戳原样穿过。用于比对「子图展开 == 手写扁平」。
fn drive_passthrough(graph: &Graph) {
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    assert_eq!(graph.state(), State::Running);

    let input = graph.input("in").unwrap();
    let mut got = Vec::new();
    for i in 0..10i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        let p = poller.next().expect("should have output");
        got.push((*p.get::<i32>().unwrap(), p.timestamp().0));
    }
    assert_eq!(
        got,
        (0..10).map(|i| (i, i as i64)).collect::<Vec<_>>(),
        "value+timestamp should pass through the expanded subgraph unchanged"
    );

    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);
}

/// 单个子图实例展开成等价的两级直通链(对照 e2e.rs::passthrough_pipeline)。
#[test]
fn subgraph_expands_to_equivalent_pipeline() {
    init();
    let graph = Graph::from_yaml(
        r#"
subgraphs:
  PassPair:
    nodes:
      - { name: a, kernel: PassThroughKernel, input_ports: ["sin"], output_ports: ["mid"] }
      - { name: b, kernel: PassThroughKernel, input_ports: ["mid"], output_ports: ["sout"] }
    input_ports: ["sin"]
    output_ports: ["sout"]
nodes:
  - { name: p, type: PassPair, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    drive_passthrough(&graph);
}

/// 嵌套子图:Outer 内部实例化 Inner,Inner 内部才是真正的算子。递归展开成单节点直通。
#[test]
fn nested_subgraph_expands() {
    init();
    let graph = Graph::from_yaml(
        r#"
subgraphs:
  Inner:
    nodes:
      - { name: k, kernel: PassThroughKernel, input_ports: ["i"], output_ports: ["o"] }
    input_ports: ["i"]
    output_ports: ["o"]
  Outer:
    nodes:
      - { name: x, type: Inner, input_ports: ["oi"], output_ports: ["oo"] }
    input_ports: ["oi"]
    output_ports: ["oo"]
nodes:
  - { name: p, type: Outer, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    drive_passthrough(&graph);
}

/// 同一子图实例化两次:命名空间隔离各自的内部边,两条链独立跑通。
#[test]
fn subgraph_instantiated_twice_is_namespaced() {
    init();
    // in --p--> mid --q--> out,p、q 都是 PassPair 实例。
    let graph = Graph::from_yaml(
        r#"
subgraphs:
  PassPair:
    nodes:
      - { name: a, kernel: PassThroughKernel, input_ports: ["sin"], output_ports: ["m"] }
      - { name: b, kernel: PassThroughKernel, input_ports: ["m"], output_ports: ["sout"] }
    input_ports: ["sin"]
    output_ports: ["sout"]
nodes:
  - { name: p, type: PassPair, input_ports: ["in"], output_ports: ["mid"] }
  - { name: q, type: PassPair, input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .expect("two instances of the same subgraph must namespace their internal edge `m`");
    drive_passthrough(&graph);
}

/// `include` 从独立文件引入子图库,主图用 `type:` 实例化。
#[test]
fn include_merges_subgraph_library_from_file() {
    init();
    let dir = std::env::temp_dir().join("lmflow_include_e2e_ok");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.yml"),
        r#"
subgraphs:
  PassPair:
    nodes:
      - { name: a, kernel: PassThroughKernel, input_ports: ["sin"], output_ports: ["mid"] }
      - { name: b, kernel: PassThroughKernel, input_ports: ["mid"], output_ports: ["sout"] }
    input_ports: ["sin"]
    output_ports: ["sout"]
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("main.yml"),
        r#"
include: ["lib.yml"]
nodes:
  - { name: p, type: PassPair, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();

    let graph = Graph::from_yaml_file(dir.join("main.yml").to_str().unwrap()).unwrap();
    drive_passthrough(&graph);
    let _ = std::fs::remove_dir_all(&dir);
}

/// include 指向不存在的文件 → 报错(不静默)。
#[test]
fn missing_include_file_is_rejected() {
    init();
    let dir = std::env::temp_dir().join("lmflow_include_e2e_missing");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("main.yml"),
        r#"
include: ["nope.yml"]
nodes:
  - { name: p, kernel: PassThroughKernel, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let err = Graph::from_yaml_file(dir.join("main.yml").to_str().unwrap()).unwrap_err();
    assert!(
        format!("{err:?}").contains("nope.yml"),
        "missing include must be reported: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 文本入口不支持 include(相对路径无从解析)→ 明确报错,指向 from_yaml_file。
#[test]
fn include_in_text_is_rejected() {
    let err = Graph::from_yaml(
        r#"
include: ["lib.yml"]
nodes:
  - { name: p, kernel: PassThroughKernel, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(
        format!("{err:?}").contains("from_yaml_file"),
        "text-mode include must point users at from_yaml_file: {err:?}"
    );
}

/// DOT 导出:子图命名空间还原成 cluster,执行器上色 + 绑核进图例。
#[test]
fn to_dot_renders_subgraph_clusters_and_executor_affinity() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2, affinity: [0, 1] }
subgraphs:
  PassPair:
    nodes:
      - { name: a, kernel: PassThroughKernel, executor: cpu, input_ports: ["sin"], output_ports: ["mid"] }
      - { name: b, kernel: PassThroughKernel, input_ports: ["mid"], output_ports: ["sout"] }
    input_ports: ["sin"]
    output_ports: ["sout"]
nodes:
  - { name: p, type: PassPair, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let dot = graph.to_dot();

    assert!(dot.contains("digraph lmflow"), "{dot}");
    assert!(dot.contains("rankdir=LR"), "{dot}");
    // 子图 p 还原成 cluster,标签为命名空间名 p
    assert!(
        dot.contains("subgraph cluster_"),
        "expected a namespace cluster:\n{dot}"
    );
    assert!(
        dot.contains("label=\"p\""),
        "cluster should be labelled p:\n{dot}"
    );
    // 执行器落位:p/a 在 cpu,p/b 在主线程
    assert!(dot.contains("@cpu"), "{dot}");
    assert!(dot.contains("@main"), "{dot}");
    // 图例含线程数与绑定核(线程亲和度可视化)
    assert!(dot.contains("cluster_legend"), "{dot}");
    assert!(
        dot.contains("cores[0,1]"),
        "affinity cores must be shown:\n{dot}"
    );
    assert!(dot.contains("2t"), "thread count must be shown:\n{dot}");
    // 内部边被命名空间化为 p/mid,并作为边标注出现
    assert!(
        dot.contains("label=\"p/mid\""),
        "namespaced internal edge label:\n{dot}"
    );
    // 图输入 / 输出口
    assert!(
        dot.contains("shape=cds"),
        "graph ports drawn as distinct shape:\n{dot}"
    );
}

/// 无子图、无执行器的普通图:不产生任何 cluster(聚簇仅在有命名空间时出现)。
#[test]
fn to_dot_plain_graph_has_no_clusters() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: n1, kernel: PassThroughKernel, input_ports: ["in"], output_ports: ["mid"] }
  - { name: n2, kernel: PassThroughKernel, input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let dot = graph.to_dot();
    assert!(dot.contains("digraph lmflow"), "{dot}");
    assert!(
        !dot.contains("cluster_"),
        "plain graph must have no clusters:\n{dot}"
    );
    assert!(
        dot.contains("@main"),
        "nodes run on the host main thread:\n{dot}"
    );
}
