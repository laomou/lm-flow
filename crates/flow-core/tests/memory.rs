//! 内存与不变量测试。
//!
//! 没有 ASan/valgrind 也能精确验证的两件事:
//!  1. **所有权守恒** —— 外部 payload 的 `drop_fn` 恰好被调用一次(不漏不重)。
//!     用一个存活计数器直接记账,比事后扫内存更精确,也能进 CI 常跑。
//!  2. **CoW 不变量** —— 线性管线上就地改写**不发生拷贝**。
//!     `docs/design.md` §3.4 专门警告过:引擎若多留一份引用,CoW 会静默退化成
//!     每帧全拷贝 —— 不报错、只变慢。所以必须有测试钉住它。

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use flow_core::packet::{dtype, BufferData, Builtin};
use flow_core::{Graph, Packet, Timestamp};

/// 外部 payload 的存活数。创建 +1,drop_fn 被调 -1。
static ALIVE: AtomicIsize = AtomicIsize::new(0);

/// 计数器是进程级的,而 cargo 默认并行跑测试 —— 不串行化就会互相串扰,
/// 把别的测试的在途包误判成本测试的泄漏。凡是用 ALIVE 的测试都必须持此锁。
static ACCOUNTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取得记账锁并返回当前基线。
fn accounting() -> (std::sync::MutexGuard<'static, ()>, isize) {
    let g = ACCOUNTING.lock().unwrap_or_else(|e| e.into_inner());
    let base = ALIVE.load(Ordering::SeqCst);
    (g, base)
}

unsafe extern "C" fn tracked_drop(p: *mut c_void) {
    ALIVE.fetch_sub(1, Ordering::SeqCst);
    drop(Box::from_raw(p as *mut i32));
}

fn tracked_packet(v: i32, ts: i64) -> Packet {
    ALIVE.fetch_add(1, Ordering::SeqCst);
    let ptr = Box::into_raw(Box::new(v)) as *mut c_void;
    unsafe { Packet::from_foreign(ptr, 0, Some(tracked_drop)) }.at(Timestamp(ts))
}

fn linear_graph(kernel: &str) -> Graph {
    flow_core::register_builtin_kernels();
    Graph::from_yaml(&format!(
        r#"
nodes:
  - {{ name: "a", kernel: "{kernel}", input_ports: ["in"], output_ports: ["mid"] }}
  - {{ name: "b", kernel: "{kernel}", input_ports: ["mid"], output_ports: ["out"] }}
input_ports: ["in"]
output_ports: ["out"]
"#
    ))
    .unwrap()
}

#[test]
fn foreign_payloads_are_released_exactly_once() {
    let (_lock, base) = accounting();
    {
        let graph = linear_graph("PassThroughKernel");
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();

        for i in 0..20i32 {
            input.send(tracked_packet(i, i as i64)).unwrap();
            let p = poller.next().expect("应有输出");
            drop(p); // 宿主用完即释放
        }
        assert_eq!(ALIVE.load(Ordering::SeqCst), base, "取走并释放后不应有残留");

        graph.close_all_inputs();
        graph.wait_done().unwrap();
    }
    assert_eq!(ALIVE.load(Ordering::SeqCst), base, "图销毁后必须全部归还");
}

#[test]
fn packets_left_in_queues_are_released_on_graph_drop() {
    let (_lock, base) = accounting();
    {
        let graph = linear_graph("PassThroughKernel");
        graph.start().unwrap(); // 不挂 poller,包会积压在管线里
        let input = graph.input("in").unwrap();
        for i in 0..10i32 {
            input.send(tracked_packet(i, i as i64)).unwrap();
        }
        assert!(
            ALIVE.load(Ordering::SeqCst) > base,
            "此刻应有在途包(否则本测试没测到东西)"
        );
        // 直接丢弃图,不排空 —— 这是最容易漏释放的路径
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "未排空就销毁图,在途包也必须释放"
    );
}

#[test]
fn packets_are_released_when_kernel_fails() {
    let (_lock, base) = accounting();
    {
        flow_core::register_builtin_kernels();
        // ScaleKernel 契约声明 int;送 type_id=0 会被类型校验拒绝 → 走失败路径
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
        graph
            .input("in")
            .unwrap()
            .send(tracked_packet(1, 0))
            .unwrap();
        graph.close_all_inputs();
        assert!(graph.wait_done().is_err(), "类型不符应报错");
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "失败路径同样不得泄漏(staging 被丢弃时包必须释放)"
    );
}

#[test]
fn cancel_path_releases_everything() {
    let (_lock, base) = accounting();
    {
        let graph = linear_graph("PassThroughKernel");
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..5i32 {
            input.send(tracked_packet(i, i as i64)).unwrap();
        }
        graph.cancel();
        let _ = graph.wait_done();
    }
    assert_eq!(ALIVE.load(Ordering::SeqCst), base, "取消后也必须全部归还");
}

/// **被拒的 send 也必须释放 payload**。send 把包按值收下,任何错误路径(口已关、
/// 时间戳非单调、UNSET、水位 WouldBlock 等)都得让它随函数返回而析构 —— 否则
/// 每次发送失败漏一个包。这条不变量在宿主频繁试探性发送时尤其容易被踩。
#[test]
fn rejected_send_releases_the_packet() {
    let (_lock, base) = accounting();
    {
        let graph = linear_graph("PassThroughKernel");
        graph.start().unwrap();
        let input = graph.input("in").unwrap();

        // (1) 时间戳非单调:先送 ts=5(收下),再送 ts=3(必拒)。
        input.send(tracked_packet(0, 5)).unwrap();
        let after_accept = ALIVE.load(Ordering::SeqCst);
        assert!(after_accept > base, "第一个包应在途");
        let r = input.send(tracked_packet(1, 3));
        assert!(r.is_err(), "时间戳回退必被拒");
        assert_eq!(
            ALIVE.load(Ordering::SeqCst),
            after_accept,
            "被拒的包必须立刻释放,不能叠加到在途计数上"
        );

        // (2) 往已关闭的输入口发送:必拒,且释放。
        input.close();
        let r = input.send(tracked_packet(2, 6));
        assert!(r.is_err(), "往已关闭的口发送必被拒");

        let _ = graph.wait_done();
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "所有包(含被拒的)最终都必须归还"
    );
}

/// 线程池 + `max_in_flight` 的多槽管线。并行 in-flight 的每个 context 槽都持有
/// 本次输入;取消 / 直接销毁 / 失败时,**所有槽**(不只槽 0)都必须归还 payload。
fn pool_graph_mif(kernel: &str, mif: usize) -> Graph {
    flow_core::register_builtin_kernels();
    Graph::from_yaml(&format!(
        r#"
executors:
  - {{ name: "cpu", type: "ThreadPoolExecutor", num_threads: 4 }}
nodes:
  - {{ name: "a", kernel: "{kernel}", executor: "cpu", max_in_flight: {mif},
      input_ports: ["in"], output_ports: ["out"] }}
input_ports: ["in"]
output_ports: ["out"]
"#
    ))
    .unwrap()
}

/// 取消一个正在并行处理多个时间戳的池节点:每个占用槽里的输入都必须释放。
#[test]
fn cancel_with_pool_max_in_flight_releases_all() {
    let (_lock, base) = accounting();
    {
        let graph = pool_graph_mif("PassThroughKernel", 4);
        graph.start().unwrap(); // 不挂 poller:输出会积在池里
        let input = graph.input("in").unwrap();
        for i in 0..64i32 {
            input.send(tracked_packet(i, i as i64)).unwrap();
        }
        graph.cancel();
        let _ = graph.wait_done();
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "并行 in-flight 下取消,所有槽的输入都必须归还"
    );
}

/// 不排空就直接销毁池 + 多槽节点:最容易漏掉非 0 号槽的路径。
#[test]
fn drop_undrained_pool_max_in_flight_releases_all() {
    let (_lock, base) = accounting();
    {
        let graph = pool_graph_mif("PassThroughKernel", 4);
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..64i32 {
            input.send(tracked_packet(i, i as i64)).unwrap();
        }
        // 立刻丢弃(可能仍有任务在池队列里没跑完)—— 不得漏释放任何一个槽
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "并行 in-flight 下直接销毁图,所有槽的输入都必须归还"
    );
}

/// 稳态:并行处理 300 帧后在途包应归零,不随轮次累积。
#[test]
fn steady_state_pool_max_in_flight_no_accumulation() {
    let (_lock, base) = accounting();
    let graph = pool_graph_mif("PassThroughKernel", 4);
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..300i32 {
        input.send(tracked_packet(i, i as i64)).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    let mut n = 0;
    while poller.try_next().is_some() {
        n += 1;
    }
    assert_eq!(n, 300, "并行下仍须无丢无重");
    assert_eq!(
        graph.inner().shared.total_queued(),
        0,
        "稳态下不应有残留在途包"
    );
    drop(graph);
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "并行 300 帧之后不应残留任何 payload"
    );
}

/// side packet 的 payload 必须恰好释放一次。start 时它被 clone 进**每个**节点 context 的
/// `Arc<map>`(本图 2 节点 → 连同 graph 自己的那份共 3 个 Arc 引用),全部 drop 后才归还。
/// 靠 `Arc<Payload>` 引用计数保证:clone 只 +ref,drop_fn 只在最后一个引用消失时调一次。
#[test]
fn side_packet_payload_released_exactly_once() {
    let (_lock, base) = accounting();
    {
        let graph = linear_graph("PassThroughKernel");
        graph
            .set_side_packet("model", tracked_packet(42, 0))
            .unwrap();
        assert!(
            ALIVE.load(Ordering::SeqCst) > base,
            "注入后 side packet 应在途"
        );
        graph.start().unwrap(); // 这里把它 clone 进各 context
        graph.close_all_inputs();
        graph.wait_done().unwrap();
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "side packet 的 payload 必须恰好释放一次(不漏不重)"
    );
}

// ---------------------------------------------------------------- CoW 不变量

fn buffer_packet(len: usize, ts: i64) -> (Packet, usize) {
    let b = BufferData::new(&[2, (len / 2) as i64], dtype::U8).unwrap();
    let addr = b.bytes.as_ptr() as usize;
    (
        Packet::from_builtin(Builtin::Buffer(b)).at(Timestamp(ts)),
        addr,
    )
}

fn buffer_addr(p: &Packet) -> usize {
    match p.as_builtin() {
        Some(Builtin::Buffer(b)) => b.bytes.as_ptr() as usize,
        _ => panic!("不是缓冲包"),
    }
}

/// **核心不变量**:线性管线(单一消费者)上就地改写全程零拷贝。
///
/// 若引擎在投递后多留了一份引用,CoW 会退化成每帧复制 —— 那时输出包的缓冲地址
/// 会与输入不同,本测试即失败。这是 §3.4 那条不变量的守卫。
///
/// ⚠ 必须用**多节点**管线:单节点管线覆盖不到「上游 ctx 残留引用」这一类 bug
/// (曾真实发生:输入槽只在下次调用开头才清,导致下游 CoW 永远复制)。
#[test]
fn cow_is_zero_copy_on_linear_pipeline() {
    flow_core::register_builtin_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p1",  kernel: "PassThroughKernel", input_ports: ["in"],  output_ports: ["m1"] }
  - { name: "p2",  kernel: "PassThroughKernel", input_ports: ["m1"],  output_ports: ["m2"] }
  - { name: "inv", kernel: "InvertKernel",      input_ports: ["m2"],  output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    let (pkt, addr_in) = buffer_packet(8, 0);
    graph.input("in").unwrap().send(pkt).unwrap();
    let out = poller.next().expect("应有输出");

    assert_eq!(
        buffer_addr(&out),
        addr_in,
        "线性管线上 InvertKernel 的就地改写不应发生任何拷贝 —— \
         若此断言失败,通常意味着引擎在投递后多留了一份引用(见 docs/design.md §3.4)"
    );
    // 内容确实被改写了(0x00 取反 = 0xFF)
    match out.as_builtin() {
        Some(Builtin::Buffer(b)) => assert_eq!(b.bytes[0], 0xFF, "应已就地取反"),
        _ => panic!("不是缓冲包"),
    }

    graph.close_all_inputs();
    graph.wait_done().unwrap();
}

/// 扇出后就地改写:必须复制,且**不污染另一条分支**。
#[test]
fn cow_copies_on_fanout_without_polluting_siblings() {
    flow_core::register_builtin_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "sp",  kernel: "SplitKernel",   input_ports: ["in"], output_ports: ["a", "b"] }
  - { name: "inv", kernel: "InvertKernel",  input_ports: ["a"],  output_ports: ["oa"] }
  - { name: "pas", kernel: "PassThroughKernel", input_ports: ["b"], output_ports: ["ob"] }
input_ports: ["in"]
output_ports: ["oa", "ob"]
"#,
    )
    .unwrap();
    let pa = graph.add_poller("oa").unwrap();
    let pb = graph.add_poller("ob").unwrap();
    graph.start().unwrap();

    let (pkt, addr_in) = buffer_packet(8, 0);
    graph.input("in").unwrap().send(pkt).unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let inverted = pa.try_next().expect("分支 a");
    let untouched = pb.try_next().expect("分支 b");

    assert_ne!(
        buffer_addr(&inverted),
        addr_in,
        "被共享时就地改写必须先复制"
    );
    assert_eq!(
        buffer_addr(&untouched),
        addr_in,
        "未改写的分支应仍指向原缓冲(零拷贝)"
    );
    match (inverted.as_builtin(), untouched.as_builtin()) {
        (Some(Builtin::Buffer(x)), Some(Builtin::Buffer(y))) => {
            assert_eq!(x.bytes[0], 0xFF, "a 分支应已取反");
            assert_eq!(y.bytes[0], 0x00, "b 分支不得被污染");
        }
        _ => panic!("不是缓冲包"),
    }
}

/// 多次穿过管线不应累积内存(引用计数必须真的降回去)。
#[test]
fn steady_state_has_no_accumulation() {
    let (_lock, base) = accounting();
    flow_core::register_builtin_kernels();
    let graph = linear_graph("PassThroughKernel");
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    for i in 0..200i32 {
        input.send(tracked_packet(i, i as i64)).unwrap();
        drop(poller.next().expect("应有输出"));
    }
    // 稳态下在途包数应回到 0,而不是随轮次线性增长
    assert_eq!(
        graph.inner().shared.total_queued(),
        0,
        "稳态下不应有残留在途包"
    );
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    drop(graph);
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "200 轮之后不应残留任何 payload"
    );
}
