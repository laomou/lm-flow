//! 端到端集成测试:真实建图、真实调用 C++ 算子、真实取输出。
//!
//! 这些测试是 MVP 的验收标准,也是回归防线 —— 例如 `pump_step` 曾因
//! `if let` 临时值把 MutexGuard 拖到块结束而自锁死,`passthrough_pipeline`
//! 会立刻挂住(测试超时)而不是静默错误。

use std::sync::Mutex;

use flow_core::{Graph, Packet, State, Timestamp};

/// 日志回调是**进程级**的,cargo 又并行跑测试 —— 不串行化就会互相把回调覆盖掉,
/// 导致断言看到 0 条日志。所有使用 flow_set_log_callback 的测试都必须持此锁。
static LOG_LOCK: Mutex<()> = Mutex::new(());

fn log_guard() -> std::sync::MutexGuard<'static, ()> {
    LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn init() {
    flow_core::register_builtin_kernels();
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
        let p = poller.next().expect("应有输出");
        got.push((*p.get::<i32>().unwrap(), p.timestamp().0));
    }
    assert_eq!(
        got,
        (0..10).map(|i| (i, i as i64)).collect::<Vec<_>>(),
        "值与时间戳都应原样穿过两级直通"
    );

    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);
    assert!(poller.next().is_none(), "图结束后 poller 应返回 None");
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

    let out = poller.try_next().expect("应有输出");
    // 捆绑算子一律用内建类型 —— 因此 C++/Rust/Python 三侧都能直接读
    assert_eq!(
        out.type_id(),
        flow_core::packet::type_id::I64,
        "产出应是内建整数类型"
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
    assert!(msg.contains("类型不符"), "{msg}");
    assert!(msg.contains("输入口"), "报错应指出是哪个口: {msg}");
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
    assert_eq!(n, 3, "三个包都应到达图输出");
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

    let a = pa.try_next().expect("分支 a 应收到");
    let b = pb.try_next().expect("分支 b 应收到");
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
    assert!(p1.try_next().is_some(), "poller 1 应收到");
    assert!(p2.try_next().is_some(), "poller 2 也应独立收到一份");
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
    assert!(err.to_string().contains("成环"), "{err}");
}

/// 退化图不该崩:空图(0 节点)能建/启/关/终结。
#[test]
fn empty_graph_terminates_cleanly() {
    init();
    let graph = Graph::from_yaml("nodes: []").unwrap();
    graph.start().unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated, "空图也应能干净终结");
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
        "算子仍应处理每个包"
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
    assert!(err.to_string().contains("没有任何节点产出"), "{err}");
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
    assert!(err.to_string().contains("多个生产者"), "{err}");
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
    assert!(err.to_string().contains("找不到生产者"), "{err}");
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
    assert!(err.to_string().contains("名字冲突"), "{err}");
}

#[test]
fn rejects_source_node() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "src", kernel: "PassThroughKernel", input_ports: [], output_ports: ["out"] }
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("没有输入口"), "{err}");
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
    assert!(err.to_string().contains("未定义的 executor"), "{err}");
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
    assert!(msg.contains("未注册"), "{msg}");
    assert!(
        msg.contains("PassThroughKernel"),
        "报错应列出可用算子以指导用户: {msg}"
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
    assert_eq!(err.code(), flow_core::status::code::STATE, "{err}");
}

#[test]
fn double_start_is_state_error() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    let err = graph.start().unwrap_err();
    assert_eq!(err.code(), flow_core::status::code::STATE, "{err}");
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
    assert_eq!(err.code(), flow_core::status::code::CLOSED, "{err}");
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
    assert!(err.to_string().contains("时间戳"), "{err}");
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
    assert!(err.to_string().contains("递增"), "{err}");
    let err = input.send(Packet::new(3i32).at(Timestamp(4))).unwrap_err();
    assert!(err.to_string().contains("递增"), "{err}");
}

#[test]
fn unknown_port_names_are_not_found() {
    init();
    let graph = simple_graph();
    assert_eq!(
        graph.input("ghost").unwrap_err().code(),
        flow_core::status::code::NOT_FOUND
    );
    assert_eq!(
        graph.add_poller("ghost").unwrap_err().code(),
        flow_core::status::code::NOT_FOUND
    );
}

#[test]
fn cancel_makes_wait_done_report_cancelled() {
    init();
    let graph = simple_graph();
    graph.start().unwrap();
    graph.cancel();
    let err = graph.wait_done().unwrap_err();
    assert_eq!(err.code(), flow_core::status::code::CANCELLED, "{err}");
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
    assert!(graph.dump().contains("node"), "dump 应含节点表");
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
    assert!(!st.running, "回调已结束,不应显示 running");
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
    assert!(sent < 10, "全局水位必须能拦住无限增长,实际送进 {sent} 个");
}

/// 背压正确性:**线程池图**上,阻塞 `send` 命中全局水位时必须**等池排水**,
/// 而不是误报 `WouldBlock`。旧实现里 `pump_step` 只跑主线程任务,池图上它恒为 false,
/// 于是阻塞 send 一撞水位就直接报错 —— 本测试就是那个回归的守卫。
mod slow_sink_kernel {
    use flow_core::ffi::FlowContext;
    use std::ffi::c_void;
    use std::time::Duration;

    // 慢消费 sink(无输出口):睡 3ms 后丢弃。发端会远快于它,必然反复撞水位。
    unsafe extern "C" fn process(_s: *mut c_void, _ctx: *mut FlowContext) -> i32 {
        std::thread::sleep(Duration::from_millis(3));
        0
    }
    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let vt = flow_core::ffi::FlowKernelVTable {
                create: None,
                get_contract: None,
                open: None,
                process: Some(process),
                close: None,
                destroy: None,
            };
            let vt: &'static _ = Box::leak(Box::new(vt));
            let name = std::ffi::CString::new("SlowSink").unwrap();
            let rc = unsafe {
                flow_core::ffi::flow_register_kernel(name.as_ptr(), vt, std::ptr::null_mut())
            };
            assert_eq!(rc, 0, "注册 SlowSink 失败");
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
            .unwrap_or_else(|e| panic!("阻塞 send 第 {i} 个不应失败(应转为背压等待),却得到 {e}"));
    }
    graph.close_all_inputs();
    graph
        .wait_done_timeout(std::time::Duration::from_secs(30))
        .unwrap();

    assert_eq!(
        graph.node_stats(0).unwrap().processed,
        30,
        "背压下 30 个都要处理完,不丢不错"
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
    flow_core::ffi::flow_set_log_callback(Some(sink), std::ptr::null_mut());

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
    flow_core::ffi::flow_set_log_callback(None, std::ptr::null_mut());

    let msgs = MSGS.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        msgs.iter().any(|m| m.contains("watchdog")),
        "3ms 的算子在 watchdog_ms=1 下必须打 WARN,实际日志: {:?}",
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
    assert_eq!(*seen_a.lock().unwrap(), want, "observer A 应收到全部");
    assert_eq!(*seen_b.lock().unwrap(), want, "observer B 应独立收到全部");
    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(*p.get::<i32>().unwrap());
    }
    assert_eq!(got, want, "poller 也应独立收到全部");
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
    assert_eq!(graph.counter_value("sink.closed"), 0, "此刻还未关闭");

    // 故意不 close/wait,直接丢弃图句柄
    let shared = graph.shared_for_inspection();
    drop(graph);

    assert_eq!(
        shared.counter_value("sink.closed"),
        1,
        "图销毁时必须补调算子的 Close"
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
    assert!(msg.contains("calibration"), "应指出缺哪个: {msg}");
    assert!(msg.contains("norm"), "应指出是哪个节点要它: {msg}");
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
    assert!(msg.contains("scale"), "应指出是哪个参数: {msg}");
    assert!(msg.contains("norm"), "应带节点名: {msg}");
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
    assert_eq!(graph.queue_depth("in"), Some(0), "队列应已排空");

    let err = input.send(Packet::new(2i32).at(Timestamp(50))).unwrap_err();
    assert!(
        err.to_string().contains("递增"),
        "排空后仍须拒绝回退: {err}"
    );
    let err = input
        .send(Packet::new(3i32).at(Timestamp(100)))
        .unwrap_err();
    assert!(
        err.to_string().contains("递增"),
        "排空后仍须拒绝重复: {err}"
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
            WARNINGS.lock().expect("锁中毒").push(s);
        }
    }

    init();
    let _log = log_guard();
    WARNINGS.lock().expect("锁中毒").clear();
    flow_core::ffi::flow_set_log_callback(Some(sink), std::ptr::null_mut());
    let _graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["nowhere"] }
input_ports: ["in", "unused"]
"#,
    )
    .unwrap();
    flow_core::ffi::flow_set_log_callback(None, std::ptr::null_mut());

    let w = WARNINGS.lock().expect("锁中毒").join("\n");
    assert!(w.contains("unused"), "未被消费的图输入口应告警: {w}");
    assert!(w.contains("nowhere"), "产出无人接收的输出口应告警: {w}");
}
