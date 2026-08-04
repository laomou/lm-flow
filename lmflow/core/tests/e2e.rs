//! 端到端集成测试:真实建图、真实调用 C++ 算子、真实取输出。
//!
//! 这些测试是 MVP 的验收标准,也是回归防线 —— 例如 `pump_step` 曾因
//! `if let` 临时值把 MutexGuard 拖到块结束而自锁死,`passthrough_pipeline`
//! 会立刻挂住(测试超时)而不是静默错误。

#![cfg(feature = "builtin-kernels")] // 用内置 C++ 算子:纯 Rust 构建(--no-default-features)时整文件跳过

use std::sync::Mutex;

use lmflow::{Graph, Packet, State, Timestamp};

/// 日志回调是**进程级**的,cargo 又并行跑测试 —— 不串行化就会互相把回调覆盖掉,
/// 导致断言看到 0 条日志。所有使用 lmflow_set_log_callback 的测试都必须持此锁。
static LOG_LOCK: Mutex<()> = Mutex::new(());

fn log_guard() -> std::sync::MutexGuard<'static, ()> {
    LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn init() {
    lmflow::register_builtin_kernels();
}

/// 两级直通:MVP 的核心用例。
#[test]
fn passthrough_pipeline() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "n1"
    kernel: "PassThroughKernel"
    input_ports: ["in"]
    output_ports: ["mid"]
  - name: "n2"
    kernel: "PassThroughKernel"
    input_ports: ["mid"]
    output_ports: ["out"]
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();

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
        "value and timestamp should pass through both passthrough stages unchanged"
    );

    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);
    assert!(
        poller.next().is_none(),
        "poller should return None after the graph finishes"
    );
}

/// C++ 算子读 options 并产出新包 —— 跨语言按类型传值的完整链路。
///
/// 捆绑算子的契约声明的是**内建类型**(而非 C++ 的 typeid),
/// 这样同一个算子从 C++、Rust、Python 三侧都能用。
#[test]
fn cpp_kernel_reads_options_and_types() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "s"
    kernel: "ScaleKernel"
    input_ports: ["in"]
    output_ports: ["out"]
    options: { factor: 7 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(6i32 as i64).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let out = poller.try_next().expect("should have output");
    // 捆绑算子一律用内建类型 —— 因此 C++/Rust/Python 三侧都能直接读
    assert_eq!(
        out.type_id(),
        lmflow::packet::type_id::I64,
        "output should be the builtin integer type"
    );
    assert_eq!(out.as_i64(), Some(42), "6 * factor(7) = 42");
}

/// 契约声明了具体类型时,类型不符必须报错 —— 而不是让算子按错误类型解读内存。
#[test]
fn typed_contract_rejects_mismatch() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "s", kernel: "ScaleKernel", input_ports: ["in"], output_ports: ["out"], options: { factor: 2 } }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    // Rust 原生包的 type_id 是 NONE,与契约声明的 int 不符
    graph
        .input("in")
        .unwrap()
        .send(Packet::new(1i32).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    let err = graph.wait_done().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("type mismatch"), "{msg}");
    assert!(
        msg.contains("input port"),
        "the error should indicate which port: {msg}"
    );
    // `Packet::new`(Native payload)造的包是这条错误最常见的来源,而它有明确出路。
    // 错误必须把出路说出来 —— 否则读者会去翻契约,而真正该改的是造包方式。
    assert!(
        msg.contains("Rust-native")
            && msg.contains("Packet::new")
            && msg.contains("from_builtin")
            && msg.contains("new_interop"),
        "Native + NONE 的失配必须给出改用哪个 Rust 构造函数: {msg}"
    );
}

/// 同样是 `type_id == NONE`,但 payload 是 **Foreign**(C/C++ 自建、type_id 填 0)——
/// 提示必须换成 C ABI 的说法,**不能**推荐 Rust 的 `Packet::from_i64` 之类。
///
/// 存在意义:`NONE` 的来源不止 `Packet::new` —— `from_foreign(.., 0, ..)`
/// 以及 C 侧 `type_id` 填 0 的自建包都会走到这里
/// (`tests/concurrency.rs` 的 `tracked_packet` 就是前者)。此前的提示一律归因到
/// `Packet::new`,会把用不上的 Rust API 建议推给 C/C++ 宿主。
#[test]
fn typed_contract_mismatch_hint_matches_payload_kind() {
    init();
    unsafe extern "C" fn noop_drop(p: *mut std::ffi::c_void) {
        drop(unsafe { Box::from_raw(p as *mut i64) });
    }
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "s", kernel: "ScaleKernel", input_ports: ["in"], output_ports: ["out"], options: { factor: 2 } }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    // Foreign payload,type_id = 0(LMFLOW_TYPE_NONE)—— 模拟 C/C++ 宿主自建包
    let ptr = Box::into_raw(Box::new(7i64)) as *mut std::ffi::c_void;
    let pkt = unsafe { Packet::from_foreign(ptr, 0, Some(noop_drop)) }.at(Timestamp(0));
    graph.input("in").unwrap().send(pkt).unwrap();
    graph.close_all_inputs();
    let msg = graph.wait_done().unwrap_err().to_string();

    assert!(msg.contains("type mismatch"), "{msg}");
    assert!(
        msg.contains("LMFLOW_TYPE_NONE") && msg.contains("LMFLOW_TYPE_*"),
        "Foreign + NONE 应给 C ABI 的出路: {msg}"
    );
    assert!(
        !msg.contains("Packet::from_i64") && !msg.contains("Rust-native"),
        "不该把 Rust API 建议推给 C/C++ 宿主: {msg}"
    );
}

/// 换成带正确 type_id 的内建构造函数,同一张图就应通过 —— 与上一条配对,
/// 证明上面那条提示指的路是真的可行,而不是一句空话。
#[test]
fn builtin_constructor_satisfies_typed_contract() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "s", kernel: "ScaleKernel", input_ports: ["in"], output_ports: ["out"], options: { factor: 2 } }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(21).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(
        poller.next().and_then(|p| p.as_i64()),
        Some(42),
        "from_i64 带正确 type_id,契约应放行"
    );
}

/// 有状态算子:Sum 在 Close 时吐出总和。
#[test]
fn stateful_sum_emits_on_close() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "pass"
    kernel: "PassThroughKernel"
    input_ports: ["in"]
    output_ports: ["mid"]
  - name: "sink"
    kernel: "SinkKernel"
    input_ports: ["mid"]
    output_ports: []
input_ports: ["in"]
output_ports: ["mid"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("mid").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..3i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let mut n = 0;
    while poller.try_next().is_some() {
        n += 1;
    }
    assert_eq!(n, 3, "all three packets should reach the graph output");
}

/// 扇出:Split 一进多出,两条分支各自收到每个包(共享 payload,不拷贝)。
#[test]
fn fanout_delivers_to_every_consumer() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "sp"
    kernel: "SplitKernel"
    input_ports: ["in"]
    output_ports: ["a", "b"]
  - name: "pa"
    kernel: "PassThroughKernel"
    input_ports: ["a"]
    output_ports: ["oa"]
  - name: "pb"
    kernel: "PassThroughKernel"
    input_ports: ["b"]
    output_ports: ["ob"]
input_ports: ["in"]
output_ports: ["oa", "ob"]
"#,
    )
    .unwrap();
    let pa = graph.add_poller("oa").unwrap();
    let pb = graph.add_poller("ob").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::new(42i32).at(Timestamp(0))).unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let a = pa.try_next().expect("branch a should receive");
    let b = pb.try_next().expect("branch b should receive");
    assert_eq!(a.get::<i32>(), Some(&42));
    assert_eq!(b.get::<i32>(), Some(&42));
}

/// 同一端口挂多个 poller:各自独立收一份。
#[test]
fn multiple_pollers_on_same_port() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let p1 = graph.add_poller("out").unwrap();
    let p2 = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::new(1i32).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert!(p1.try_next().is_some(), "poller 1 should receive");
    assert!(
        p2.try_next().is_some(),
        "poller 2 should also independently receive a copy"
    );
}

// ---------------------------------------------------------------- 校验与错误路径

#[test]
fn rejects_cycle() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "a", kernel: "PassThroughKernel", input_ports: ["y"], output_ports: ["x"] }
  - { name: "b", kernel: "PassThroughKernel", input_ports: ["x"], output_ports: ["y"] }
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("cycle"), "{err}");
}

/// 退化图不该崩:空图(0 节点)能建/启/关/终结。
#[test]
fn empty_graph_terminates_cleanly() {
    init();
    let graph = Graph::from_yaml("nodes: []").unwrap();
    graph.start().unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(
        graph.state(),
        State::Terminated,
        "an empty graph should also terminate cleanly"
    );
}

/// 节点输出无人消费、也不是图输出口(悬空输出):合法,包被丢弃,不崩不泄漏。
#[test]
fn dangling_output_is_allowed() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "n", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["dangling"] }
input_ports: ["in"]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..5i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(
        graph.node_stats(0).unwrap().processed,
        5,
        "the kernel should still process every packet"
    );
}

/// 图输出口没有任何节点产出它:必须在 init 报错(而不是留个永不触发的 poller)。
#[test]
fn rejects_graph_output_with_no_producer() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "n", kernel: "SinkKernel", input_ports: ["in"], output_ports: [] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no node produces"), "{err}");
}

/// MuxKernel:控制口(输入 0)的 I64 值选择转发哪个数据口(输入 1..)。默认 sync 全对齐。
#[test]
fn mux_kernel_forwards_selected_data_port() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "m"
    kernel: "MuxKernel"
    input_ports: ["sel", "a", "b"]
    output_ports: ["out"]
input_ports: ["sel", "a", "b"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let sel = graph.input("sel").unwrap();
    let a = graph.input("a").unwrap();
    let b = graph.input("b").unwrap();
    // sync 全对齐:每个 ts 三口都要有包。ts0 选 0→转发 a;ts1 选 1→转发 b。
    for (ts, k, av, bv) in [(0i64, 0i64, 100i64, 200i64), (1, 1, 101, 201)] {
        sel.send(Packet::from_i64(k).at(Timestamp(ts))).unwrap();
        a.send(Packet::from_i64(av).at(Timestamp(ts))).unwrap();
        b.send(Packet::from_i64(bv).at(Timestamp(ts))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    let mut got = Vec::new();
    while let Some(p) = out.try_next() {
        got.push(p.as_i64().unwrap());
    }
    assert_eq!(
        got,
        vec![100, 201],
        "ts0 selects data port 0 (a=100), ts1 selects data port 1 (b=201)"
    );
}

/// MuxKernel:选择器越界必须报错(不静默转发错口/崩溃)。
#[test]
fn mux_kernel_rejects_out_of_range_selector() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "m", kernel: "MuxKernel", input_ports: ["sel", "a"], output_ports: ["out"] }
input_ports: ["sel", "a"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    // 只有 1 个数据口(a),选择器 5 越界
    graph
        .input("sel")
        .unwrap()
        .send(Packet::from_i64(5).at(Timestamp(0)))
        .unwrap();
    graph
        .input("a")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    let err = graph.wait_done().unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
fn rejects_duplicate_producer() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "a", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["dup"] }
  - { name: "b", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["dup"] }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("multiple producers"), "{err}");
}

#[test]
fn rejects_unconnected_input() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "a", kernel: "PassThroughKernel", input_ports: ["nowhere"], output_ports: ["out"] }
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no producer"), "{err}");
}

#[test]
fn rejects_name_clash_between_graph_input_and_node_output() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "a", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["in"] }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("name conflict"), "{err}");
}

#[test]
fn source_node_requires_executor() {
    init();
    // 0 输入 = 源节点:必须挂线程池 executor(否则会独占宿主主线程、拖垮全图)。
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "src", kernel: "RangeSourceKernel", input_ports: [], output_ports: ["out"] }
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("requires an executor"), "{err}");
}

#[test]
fn source_node_produces_and_terminates() {
    init();
    // 源算子(0 输入)产 0..count,发完 SourceDone → 图自然终止;源挂线程池。
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - { name: "src", kernel: "RangeSourceKernel", input_ports: [], output_ports: ["out"], executor: "cpu", options: { count: 5 } }
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    // 没有输入口可喂 —— 源自产。等图跑完(有限源会自报完成)。
    graph
        .wait_done_timeout(std::time::Duration::from_secs(30))
        .unwrap();
    assert_eq!(graph.state(), State::Terminated);

    let mut got = Vec::new();
    while let Some(p) = out.try_next() {
        got.push(p.as_i64().expect("i64 packet"));
    }
    assert_eq!(
        got,
        vec![0, 1, 2, 3, 4],
        "source should emit 0..count in order"
    );
}

#[test]
fn rejects_unknown_executor() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "a", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"], executor: "ghost" }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("undefined executor"), "{err}");
}

#[test]
fn rejects_unregistered_kernel_and_lists_available() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "a", kernel: "NoSuchKernel", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not registered"), "{msg}");
    assert!(
        msg.contains("PassThroughKernel"),
        "the error should list available kernels to guide the user: {msg}"
    );
}

// ---------------------------------------------------------------- 状态机

#[test]
fn send_before_start_is_state_error() {
    init();
    let graph = simple_graph();
    let err = graph
        .input("in")
        .unwrap()
        .send(Packet::new(1i32).at(Timestamp(0)))
        .unwrap_err();
    assert_eq!(err.code(), lmflow::status::code::STATE, "{err}");
}

#[test]
fn double_start_is_state_error() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    let err = graph.start().unwrap_err();
    assert_eq!(err.code(), lmflow::status::code::STATE, "{err}");
}

#[test]
fn add_poller_after_start_is_rejected() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    // start 之后再挂 poller 会漏掉已产出的包,故直接拒绝
    assert!(graph.add_poller("out").is_err());
}

#[test]
fn send_after_close_is_closed_error() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    graph.close_all_inputs();
    let err = input.send(Packet::new(1i32).at(Timestamp(0))).unwrap_err();
    assert_eq!(err.code(), lmflow::status::code::CLOSED, "{err}");
}

#[test]
fn unset_timestamp_on_graph_input_is_rejected() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    let err = graph
        .input("in")
        .unwrap()
        .send(Packet::new(1i32)) // 未调 .at()
        .unwrap_err();
    assert!(err.to_string().contains("timestamp"), "{err}");
}

#[test]
fn timestamps_must_be_strictly_increasing() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::new(1i32).at(Timestamp(5))).unwrap();
    // 同一时间戳或回退都应被拒 —— 乱序会让下游行为诡异
    let err = input.send(Packet::new(2i32).at(Timestamp(5))).unwrap_err();
    assert!(err.to_string().contains("increasing"), "{err}");
    let err = input.send(Packet::new(3i32).at(Timestamp(4))).unwrap_err();
    assert!(err.to_string().contains("increasing"), "{err}");
}

#[test]
fn unknown_port_names_are_not_found() {
    init();
    let graph = simple_graph();
    assert_eq!(
        graph.input("ghost").unwrap_err().code(),
        lmflow::status::code::NOT_FOUND
    );
    assert_eq!(
        graph.add_poller("ghost").unwrap_err().code(),
        lmflow::status::code::NOT_FOUND
    );
}

#[test]
fn cancel_makes_wait_done_report_cancelled() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    graph.cancel();
    let err = graph.wait_done().unwrap_err();
    assert_eq!(err.code(), lmflow::status::code::CANCELLED, "{err}");
}

// ---------------------------------------------------------------- 内省

#[test]
fn introspection_reports_topology() {
    init();
    let graph = simple_graph();
    assert_eq!(graph.node_count(), 1);
    assert_eq!(graph.node_name(0), Some("p"));
    assert_eq!(graph.input_port_names(), vec!["in"]);
    assert_eq!(graph.output_port_names(), vec!["out"]);
    assert_eq!(graph.queue_depth("in"), Some(0));
    assert!(
        graph.dump().contains("node"),
        "dump should contain the node table"
    );
}

#[test]
fn node_stats_track_processing() {
    init();
    let graph = simple_graph();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..3i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        poller.next().unwrap();
    }
    let st = graph.node_stats(0).unwrap();
    assert_eq!(st.node_name, "p");
    assert_eq!(st.kernel_name, "PassThroughKernel");
    assert_eq!(st.processed, 3);
    assert_eq!(st.errors, 0);
    assert!(
        !st.running,
        "callback has finished, should not show running"
    );
}

#[test]
fn global_watermark_blocks_input_when_exceeded() {
    init();
    // 上限 2 个在途包,且下游不消费(sink 无输出口但会消费;这里用未连接的中间边制造积压)
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
max_queued_packets: 2
"#,
    )
    .unwrap();
    graph.start().unwrap(); // 不挂 poller,输出包不会被取走
    let input = graph.input("in").unwrap();
    // 前两个能进;之后水位到顶,try_send 应拒绝而不是无限增长
    let mut sent = 0;
    for i in 0..10i32 {
        if input
            .try_send(Packet::new(i).at(Timestamp(i as i64)))
            .is_err()
        {
            break;
        }
        sent += 1;
    }
    assert!(
        sent < 10,
        "the global watermark must stop unbounded growth, actually sent {sent}"
    );
}

/// 背压正确性:**线程池图**上,阻塞 `send` 命中全局水位时必须**等池排水**,
/// 而不是误报 `WouldBlock`。旧实现里 `pump_step` 只跑主线程任务,池图上它恒为 false,
/// 于是阻塞 send 一撞水位就直接报错 —— 本测试就是那个回归的守卫。
mod slow_sink_kernel {
    use lmflow::ffi::LMFlowContext;
    use std::ffi::c_void;
    use std::time::Duration;

    // 慢消费 sink(无输出口):睡 3ms 后丢弃。发端会远快于它,必然反复撞水位。
    unsafe extern "C" fn process(_s: *mut c_void, _ctx: *mut LMFlowContext) -> i32 {
        std::thread::sleep(Duration::from_millis(3));
        0
    }
    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let vt = lmflow::ffi::LMFlowKernelVTable {
                create: None,
                get_contract: None,
                open: None,
                process: Some(process),
                close: None,
                destroy: None,
            };
            // 引擎在 register 内按值拷贝 vtable,返回后不再引用 —— 故栈上量即可,无需泄漏。
            let name = std::ffi::CString::new("SlowSink").unwrap();
            let rc = unsafe {
                lmflow::ffi::lmflow_register_kernel(name.as_ptr(), &vt, std::ptr::null_mut())
            };
            assert_eq!(rc, 0, "failed to register SlowSink");
        });
    }
}

#[test]
fn blocking_send_applies_backpressure_on_pool_instead_of_erroring() {
    init();
    slow_sink_kernel::register();
    // 池 2 线程、每包 3ms;发端远快于处理端,必然反复撞上 max_queued_packets 水位。
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - { name: "s", kernel: "SlowSink", executor: "cpu", input_ports: ["in"], output_ports: [] }
input_ports: ["in"]
max_queued_packets: 4
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    // 30 个阻塞 send:命中水位时必须转为背压等待,而不是报 WouldBlock。
    // 旧实现会在这里 panic(池图上 pump_step 恒 false → 直接 WouldBlock)。
    for i in 0..30i32 {
        input
            .send(Packet::new(i).at(Timestamp(i as i64)))
            .unwrap_or_else(|e| panic!("blocking send #{i} should not fail (should become backpressure waiting), but got {e}"));
    }
    graph.close_all_inputs();
    graph
        .wait_done_timeout(std::time::Duration::from_secs(30))
        .unwrap();

    assert_eq!(
        graph.node_stats(0).unwrap().processed,
        30,
        "under backpressure all 30 must be processed, no loss no error"
    );
}

/// watchdog:单次算子回调超过阈值必须打 WARN(卡死/慢帧可观测)。
#[test]
fn watchdog_warns_on_slow_kernel() {
    use std::ffi::{c_char, c_void, CStr};
    let _lg = log_guard(); // 日志回调是进程级的,串行化
    init();
    slow_sink_kernel::register(); // SlowSink 睡 3ms

    static MSGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    MSGS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    unsafe extern "C" fn sink(_u: *mut c_void, _lv: i32, msg: *const c_char) {
        if !msg.is_null() {
            let s = unsafe { CStr::from_ptr(msg) }
                .to_string_lossy()
                .into_owned();
            MSGS.lock().unwrap_or_else(|e| e.into_inner()).push(s);
        }
    }
    lmflow::ffi::lmflow_set_log_callback(Some(sink), std::ptr::null_mut());

    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 1 }
nodes:
  - { name: "s", kernel: "SlowSink", executor: "cpu", input_ports: ["in"], output_ports: [] }
input_ports: ["in"]
watchdog_ms: 1
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::new(0i32).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph
        .wait_done_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    lmflow::ffi::lmflow_set_log_callback(None, std::ptr::null_mut());

    let msgs = MSGS.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        msgs.iter().any(|m| m.contains("watchdog")),
        "a 3ms kernel under watchdog_ms=1 must emit a WARN, actual logs: {:?}",
        *msgs
    );
}

// ---------------------------------------------------------------- 辅助

fn simple_graph() -> Graph {
    Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap()
}

/// 同一端口挂多个 observer + 一个 poller:每个订阅者都**独立**收到全部包(不漏不串)。
/// 也顺带覆盖「快照订阅者后释放锁再回调」这条路径。
#[test]
fn multiple_observers_and_poller_all_receive() {
    use std::sync::{Arc, Mutex};
    init();
    let graph = simple_graph();
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let a = seen_a.clone();
    let b = seen_b.clone();
    graph
        .observe("out", move |p| {
            a.lock().unwrap().push(*p.get::<i32>().unwrap())
        })
        .unwrap();
    graph
        .observe("out", move |p| {
            b.lock().unwrap().push(*p.get::<i32>().unwrap())
        })
        .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..5i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let want: Vec<i32> = (0..5).collect();
    assert_eq!(
        *seen_a.lock().unwrap(),
        want,
        "observer A should receive all"
    );
    assert_eq!(
        *seen_b.lock().unwrap(),
        want,
        "observer B should independently receive all"
    );
    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(*p.get::<i32>().unwrap());
    }
    assert_eq!(got, want, "poller should also independently receive all");
}

// ---------------------------------------------------------------- 兜底关流与 side packet

/// 图被直接丢弃(未走 wait_done)时,已 open 的算子仍必须收到 Close ——
/// 否则算子里申请的资源(文件、连接、GPU 上下文)不会被释放。
///
/// 用**按图的计数器**断言:全局日志接收器会被并发跑的其它测试干扰。
/// 计数器随图一起销毁,所以必须在 drop 之前读 —— 这里靠 Poller 延长 GraphInner 寿命,
/// 于是 Graph 句柄销毁(触发兜底 Close)之后仍能读到计数。
#[test]
fn dropping_graph_still_closes_opened_kernels() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "s", kernel: "SinkKernel", input_ports: ["in"], output_ports: [] }
input_ports: ["in"]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::new(1i32).at(Timestamp(0)))
        .unwrap();
    assert_eq!(
        graph.counter_value("sink.closed"),
        0,
        "not closed yet at this point"
    );

    // 故意不 close/wait,直接丢弃图句柄
    let shared = graph.shared_for_inspection();
    drop(graph);

    assert_eq!(
        shared.counter_value("sink.closed"),
        1,
        "graph destruction must still call the kernel's Close"
    );
}

/// 算子声明的必需 side packet 未注入时,start 阶段就该报错并指出是哪个节点要它。
#[test]
fn missing_required_side_packet_is_rejected_at_start() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "norm"
    kernel: "NormalizeKernel"
    input_ports: ["in"]
    output_ports: ["out"]
    options: { scale: 0.5 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let err = graph.start().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("calibration"),
        "should indicate which one is missing: {msg}"
    );
    assert!(
        msg.contains("norm"),
        "should indicate which node requires it: {msg}"
    );
}

/// 注入之后即可正常启动,且算子能读到。
#[test]
fn side_packet_is_visible_to_kernel() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "norm"
    kernel: "NormalizeKernel"
    input_ports: ["in"]
    output_ports: ["out"]
    options: { scale: 0.5, mean: [1.0, 2.0] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph
        .set_side_packet("calibration", Packet::new(vec![1.0f64, 2.0]))
        .unwrap();
    graph.start().unwrap();
    // start 之后再注入应被拒
    assert!(graph.set_side_packet("late", Packet::new(0u8)).is_err());
    graph.close_all_inputs();
    graph.wait_done().unwrap();
}

/// 必需参数缺失时,算子在 Open 里失败并带上可读原因。
#[test]
fn missing_required_option_fails_with_reason() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "norm"
    kernel: "NormalizeKernel"
    input_ports: ["in"]
    output_ports: ["out"]
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph
        .set_side_packet("calibration", Packet::new(0u8))
        .unwrap();
    // options.scale 未配 —— NormalizeKernel 用 RequireOption 读它
    let err = graph.start().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("scale"),
        "should indicate which parameter: {msg}"
    );
    assert!(msg.contains("norm"), "should carry the node name: {msg}");
}

/// 时间戳单调性必须**独立记录参照值**:队列排空后回退的时间戳同样要被拒。
/// (曾经的实现只跟队列里剩下的包比较,一排空校验就失效了。)
#[test]
fn timestamp_monotonicity_survives_queue_drain() {
    init();
    let graph = simple_graph();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    input.send(Packet::new(1i32).at(Timestamp(100))).unwrap();
    // 把队列彻底排空 —— 此后若参照值来自队列,就没有参照可比了
    assert!(poller.next().is_some());
    assert_eq!(
        graph.queue_depth("in"),
        Some(0),
        "the queue should be drained"
    );

    let err = input.send(Packet::new(2i32).at(Timestamp(50))).unwrap_err();
    assert!(
        err.to_string().contains("increasing"),
        "must still reject going backward after drain: {err}"
    );
    let err = input
        .send(Packet::new(3i32).at(Timestamp(100)))
        .unwrap_err();
    assert!(
        err.to_string().contains("increasing"),
        "must still reject duplicates after drain: {err}"
    );
    // 更大的时间戳仍可继续
    input.send(Packet::new(4i32).at(Timestamp(101))).unwrap();
}

/// 无人消费的端口会静默丢包 —— 引擎必须出声告警。
#[test]
fn warns_about_unconsumed_ports() {
    use std::ffi::{c_char, c_void, CStr};
    use std::sync::Mutex;

    static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    unsafe extern "C" fn sink(_u: *mut c_void, level: i32, msg: *const c_char) {
        if level == 1 {
            let s = unsafe { CStr::from_ptr(msg) }
                .to_string_lossy()
                .into_owned();
            WARNINGS.lock().expect("lock poisoned").push(s);
        }
    }

    init();
    let _log = log_guard();
    WARNINGS.lock().expect("lock poisoned").clear();
    lmflow::ffi::lmflow_set_log_callback(Some(sink), std::ptr::null_mut());
    let _graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["nowhere"] }
input_ports: ["in", "unused"]
"#,
    )
    .unwrap();
    lmflow::ffi::lmflow_set_log_callback(None, std::ptr::null_mut());

    let w = WARNINGS.lock().expect("lock poisoned").join("\n");
    assert!(
        w.contains("unused"),
        "an unconsumed graph input port should warn: {w}"
    );
    assert!(
        w.contains("nowhere"),
        "an output port with no receiver should warn: {w}"
    );
}
