//! 并发测试:线程池执行器。
//!
//! 这些测试是 M3 的验收标准。重点不是「能跑」,而是几件容易出错的事:
//!  * 节点真的跑在了**别的线程**上(不是静默退回主线程);
//!  * 阻塞接口在等线程池时不会**提前返回**,也不会**永久挂住**;
//!  * 并发下算子的 `Close` 只被调用一次(宿主线程与工作线程会同时想关它);
//!  * 所有权守恒在并发下仍然成立;
//!  * 混合执行器(一部分节点在池里、一部分交还宿主线程)不会死锁。

mod common;

use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet, Timestamp};

fn init() {
    common::register_test_kernels();
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<DelegatedSlowPass>("DelegatedSlowPass").unwrap();
        register_kernel::<FiniteSource>("FiniteSource").unwrap();
    });
}

static DELEGATED_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static DELEGATED_MAX_ACTIVE: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct DelegatedSlowPass;

impl Kernel for DelegatedSlowPass {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let active = DELEGATED_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        DELEGATED_MAX_ACTIVE.fetch_max(active, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(5));
        DELEGATED_ACTIVE.fetch_sub(1, Ordering::SeqCst);
        cc.forward(0, 0)
    }
}

#[derive(Default)]
struct FiniteSource {
    emitted: bool,
}

impl Kernel for FiniteSource {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        if self.emitted {
            context.source_done();
        } else {
            context.emit(0, Packet::from_i64(1))?;
            self.emitted = true;
        }
        Ok(())
    }
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
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    // 默认执行器恒在首位 —— 它是引擎补的,不写 executor 的 "p" 就归它。
    assert_eq!(g.executor_names(), vec!["default", "cpu"]);
}

/// `executors` 里写的一律是宿主自己的执行器:必须有名字,且不能占用引擎保留的 `default`。
#[test]
fn rejects_empty_and_reserved_executor_names() {
    init();
    let err = Graph::from_yaml(
        r#"
executors:
  - { type: "ThreadPoolExecutor", num_threads: 2 }
nodes: []
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("must have a name"), "{err}");

    // `default` 是引擎隐式默认执行器的名字 —— 声明它会让一张图里出现两个 default。
    let err = Graph::from_yaml(
        r#"
executors:
  - { name: "default", type: "ThreadPoolExecutor", num_threads: 2 }
nodes: []
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("is reserved"), "{err}");
}

#[test]
fn rejects_duplicate_executors() {
    init();
    let err = Graph::from_yaml(
        r#"
executors:
  - { name: "a", type: "ThreadPoolExecutor", num_threads: 1 }
  - { name: "a", type: "ThreadPoolExecutor", num_threads: 1 }
nodes: []
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("defined more than once"), "{err}");
}

/// 全部节点跑在默认线程池上:结果必须完整且有序。
#[test]
fn all_nodes_on_default_pool_produce_correct_output() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "n1", kernel: "PassThrough", input_ports: ["in"],  output_ports: ["m1"] }
  - { name: "n2", kernel: "PassThrough", input_ports: ["m1"], output_ports: ["m2"] }
  - { name: "n3", kernel: "PassThrough", input_ports: ["m2"], output_ports: ["out"] }
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
    assert_eq!(got.len(), N as usize, "must not drop packets");
    // 单一线性链 + 每节点 max_in_flight=1,故顺序必须保持
    assert_eq!(got, (0..N).collect::<Vec<_>>(), "order must be preserved");
}

/// 回归:阻塞 `next()` 在**边关闭**与**尾包入队**同时发生时,必须先把队列排空,
/// 不能一看到 `closed` 就返回 `None` 把尾包丢掉。
///
/// `closed` 是「不会再有入队」的门闩,**不是**「队列已空」。生产者可能在消费者
/// 「pop 到空」与「读到 `closed`」这两步之间,把最后一批包入队并关边(繁忙机器上
/// 消费者恰在此处被抢占)。早先 `next()` 在这一刻直接 `Ok(None)`,静默丢尾包 ——
/// 只在 CPU 紧张时偶发(如 `peak_queue_depth_is_a_high_water_mark` 在 macOS CI 上的
/// 偶发失败)。这里用**边跑边排空**(不先 `wait_done`)重复多轮制造那个窗口;
/// 用 2 线程池 + 宿主排空线程,在少核机器上竞争最激烈。
#[test]
fn blocking_next_drains_tail_when_edge_closes_mid_drain() {
    init();
    const N: usize = 6;
    const ROUNDS: usize = 4000;
    for round in 0..ROUNDS {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: "pool", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - { name: "a", kernel: "PassThrough", executor: "pool", input_ports: ["in"],  output_ports: ["mid"] }
  - { name: "b", kernel: "PassThrough", executor: "pool", input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
        )
        .unwrap();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..N {
            input
                .send(Packet::new(i as i32).at(Timestamp(i as i64)))
                .unwrap();
        }
        graph.close_all_inputs();
        // 关键:边跑边用阻塞 next() 排空(不先 wait_done),让 pop 与「生产者入队 + 关边」竞争。
        let mut got = 0usize;
        while poller.next().is_some() {
            got += 1;
        }
        graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(
            got, N,
            "round {round}: next() 丢了尾包(closed 门闩被当成「队列已空」)"
        );
    }
}

/// 默认节点确实跑在**别的线程**上 —— 而不是静默退回主线程。
///
/// 用 Rust 的 observer 回调(**按图**注册)取代全局日志:回调在派发该包的线程上
/// 执行,于是能直接看到算子跑在哪个线程。按图隔离,不受并发跑的其它测试干扰。
#[test]
fn default_pool_nodes_actually_run_off_the_host_thread() {
    init();
    let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));

    {
        let graph = Graph::from_yaml(
            r#"
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
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
                rec.lock().expect("lock poisoned").push(name);
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

    let names = seen.lock().expect("lock poisoned").clone();
    assert_eq!(names.len(), 20, "observer should receive all 20 packets");
    assert!(
        names.iter().all(|n| n.starts_with("default-")),
        "a node without executor must run on a default-N pool thread, actual: {names:?}"
    );
}

#[test]
fn default_pool_advances_without_host_pumping() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let observed = seen.clone();
    graph
        .observe("out", move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::new(7).at(Timestamp(0)))
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while seen.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "default worker must advance after send without wait/poller/pump_step"
    );
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
}

// ------------------------------------------------------- 三类执行器落位的矩阵
//
// 节点能落在三种地方,行为差异只应体现在**并发度**与**推进时机**上;
// 「不丢包、按边保序、能终止、所有权守恒」这几条不变量三者都必须成立。
// 下面先把三种落位参数化,再对同一套场景逐一压过去 —— 免得某条路径悄悄没人测。

#[derive(Clone, Copy, Debug)]
enum Placement {
    /// 一:不写 `executor` —— 归默认执行器(引擎按 CPU 核数补的线程池)。
    Default,
    /// 二:显式指名一个具名线程池。
    ExplicitPool,
    /// 三:显式指名一个 `DelegatingExecutor` —— 交还宿主线程,零并发、顺序确定。
    /// 默认执行器是引擎持有的线程池、不可替换,所以想要宿主线程语义只能自己声明一个。
    Delegating,
}

impl Placement {
    const ALL: [Placement; 3] = [
        Placement::Default,
        Placement::ExplicitPool,
        Placement::Delegating,
    ];

    /// (`executors:` 块, 节点上要追加的 `executor` 字段)
    fn parts(self) -> (&'static str, &'static str) {
        match self {
            Placement::Default => ("", ""),
            Placement::ExplicitPool => (
                "executors:\n  - { name: \"cpu\", type: \"ThreadPoolExecutor\", num_threads: 4 }\n",
                ", executor: \"cpu\"",
            ),
            Placement::Delegating => (
                "executors:\n  - { name: \"host\", type: \"DelegatingExecutor\" }\n",
                ", executor: \"host\"",
            ),
        }
    }

    /// 三级直通管线,节点按本落位方式安放。
    fn linear_graph(self) -> Graph {
        let (execs, on_node) = self.parts();
        Graph::from_yaml(&format!(
            r#"
{execs}nodes:
  - {{ name: "n1", kernel: "PassThrough", input_ports: ["in"], output_ports: ["m1"]{on_node} }}
  - {{ name: "n2", kernel: "PassThrough", input_ports: ["m1"], output_ports: ["m2"]{on_node} }}
  - {{ name: "n3", kernel: "PassThrough", input_ports: ["m2"], output_ports: ["out"]{on_node} }}
input_ports: ["in"]
output_ports: ["out"]
"#
        ))
        .unwrap_or_else(|e| panic!("{self:?} 应当能建图: {e}"))
    }

    /// 该落位下,节点实际跑在什么线程上(用于证明落位真的生效)。
    fn expected_thread(self) -> &'static str {
        match self {
            Placement::Default => "default-",
            Placement::ExplicitPool => "cpu-",
            // 本测试只从当前宿主线程进入阻塞接口，所以委托任务也在该线程执行。
            Placement::Delegating => "HOST",
        }
    }
}

/// 三种落位都必须:不丢包 + 按边保序。
#[test]
fn every_placement_preserves_all_packets_in_order() {
    init();
    const N: i32 = 100;
    for p in Placement::ALL {
        let graph = p.linear_graph();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..N {
            input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        }
        graph.close_all_inputs();
        graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

        let mut got = Vec::new();
        while let Some(pkt) = poller.try_next() {
            got.push(*pkt.get::<i32>().unwrap());
        }
        assert_eq!(got.len(), N as usize, "{p:?} 丢包了");
        // 单一线性链 + 每节点 max_in_flight=1 ⇒ 无论落在哪,顺序都必须保持。
        assert_eq!(got, (0..N).collect::<Vec<_>>(), "{p:?} 顺序被打乱");
    }
}

/// 三种落位都必须:图能终止,且所有权守恒(拆图后一个包都不剩)。
#[test]
fn every_placement_terminates_and_conserves_ownership() {
    let _acct = ACCOUNTING.lock().unwrap_or_else(|e| e.into_inner());
    init();
    for p in Placement::ALL {
        let base = ALIVE.load(Ordering::SeqCst);
        {
            let graph = p.linear_graph();
            let poller = graph.add_poller("out").unwrap();
            graph.start().unwrap();
            let input = graph.input("in").unwrap();
            for i in 0..20i32 {
                input.send(tracked_packet(i, i as i64)).unwrap();
            }
            graph.close_all_inputs();
            graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
            while poller.try_next().is_some() {}
        }
        assert_eq!(
            ALIVE.load(Ordering::SeqCst),
            base,
            "{p:?} 拆图后仍有包没被释放"
        );
    }
}

/// 三种落位真的把节点放到了**不同的线程**上 —— 不是三条配置跑出同一种行为。
#[test]
fn every_placement_lands_on_its_own_thread() {
    init();
    let host = std::thread::current().id();
    for p in Placement::ALL {
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let graph = p.linear_graph();
            let rec = seen.clone();
            graph
                .observe("out", move |_pkt| {
                    // 本测试只由当前宿主线程推进委托任务，故顺带记线程 id 以便区分。
                    let name = if std::thread::current().id() == host {
                        "HOST".to_string()
                    } else {
                        std::thread::current()
                            .name()
                            .unwrap_or("(unnamed)")
                            .to_string()
                    };
                    rec.lock().expect("lock poisoned").push(name);
                })
                .unwrap();
            graph.start().unwrap();
            let input = graph.input("in").unwrap();
            for i in 0..5i32 {
                input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
            }
            graph.close_all_inputs();
            graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
        }
        let names = seen.lock().expect("lock poisoned").clone();
        assert_eq!(names.len(), 5, "{p:?} observer 应收到全部 5 个包");
        let want = p.expected_thread();
        // 只断言前缀,不钉死 worker 序号 —— 落在哪个 worker 上无所谓。
        assert!(
            names.iter().all(|n| n.starts_with(want)),
            "{p:?} 应跑在 `{want}*` 线程上,actual: {names:?}"
        );
    }
}

/// 三种落位在 DOT 里都各自标出来:池染色、委托留白底,且图例都有名字。
///
/// 守的是 `dot.rs` 那条 `is_delegating` 分支 —— 否则「委托执行器渲染成 0 线程的池」
/// 这类退化没人拦得住(改默认之前的 `@main` 是硬编码的,现在一律走名字)。
#[test]
fn every_placement_is_labelled_in_dot() {
    init();
    for p in Placement::ALL {
        let dot = p.linear_graph().to_dot();
        let want_label = match p {
            Placement::Default => "@default",
            Placement::ExplicitPool => "@cpu",
            Placement::Delegating => "@host",
        };
        assert!(
            dot.contains(want_label),
            "{p:?} 应标出 {want_label}:\n{dot}"
        );
        // 默认执行器恒存在,故执行器图例总会出现,且盒子里有名字(不是空标签)。
        assert!(dot.contains("cluster_legend"), "{p:?} 缺执行器图例:\n{dot}");

        let delegating_legend = dot.contains("host thread (delegating)");
        match p {
            Placement::Delegating => assert!(
                delegating_legend,
                "委托执行器的图例该写明「交还宿主线程」,而不是标成 0t 的池:\n{dot}"
            ),
            _ => assert!(
                !delegating_legend,
                "{p:?} 没有委托执行器,不该出现委托图例:\n{dot}"
            ),
        }
    }
}

// ------------------------------------------------------- 默认执行器(引擎隐式持有)
/// 默认执行器可见、有名字、是线程池。
#[test]
fn default_executor_is_a_named_thread_pool() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    assert_eq!(
        graph.executor_names(),
        vec!["default"],
        "默认执行器必须可见、有名字"
    );
}

/// 想控制线程数 / 绑核 / 优先级,就自己声明一个具名池、把节点指过去 ——
/// 默认执行器是引擎持有的,不可配(见 `rejects_empty_and_reserved_executor_names`)。
#[test]
fn own_pool_is_how_you_control_threads() {
    init();
    let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));

    {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: "solo", type: "ThreadPoolExecutor", num_threads: 1 }
nodes:
  - { name: "p", kernel: "PassThrough", executor: "solo", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
        )
        .unwrap();
        // 默认执行器仍然存在(恒在首位),只是这张图没节点用它。
        assert_eq!(graph.executor_names(), vec!["default", "solo"]);

        let rec = seen.clone();
        graph
            .observe("out", move |_pkt| {
                let name = std::thread::current()
                    .name()
                    .unwrap_or("(unnamed)")
                    .to_string();
                rec.lock().expect("lock poisoned").push(name);
            })
            .unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for i in 0..5i32 {
            input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        }
        graph.close_all_inputs();
        graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
    }

    let names = seen.lock().expect("lock poisoned").clone();
    // num_threads: 1 ⇒ 只可能有 solo-0 这一个 worker。
    assert!(
        names.iter().all(|n| n == "solo-0"),
        "num_threads: 1 应当只开一个 worker,actual: {names:?}"
    );
}

/// 委托执行器的推进时机:只 `send` 而不进阻塞接口,节点**不会推进**;进了才推进。
#[test]
fn delegating_only_advances_when_the_host_enters_the_engine() {
    init();
    let graph = Placement::Delegating.linear_graph();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..5i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    // 引擎不能凭空占宿主线程:只 send 不进引擎 ⇒ 一个包都没产出。
    assert!(
        poller.try_next().is_none(),
        "委托执行器上的节点在宿主进入引擎之前不该推进"
    );

    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(*p.get::<i32>().unwrap());
    }
    assert_eq!(
        got,
        (0..5).collect::<Vec<_>>(),
        "进了阻塞接口之后必须全部推进,且顺序确定"
    );
}

#[test]
fn multiple_delegating_executors_are_pumped_fairly() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "host_a", type: "DelegatingExecutor" }
  - { name: "host_b", type: "DelegatingExecutor" }
nodes:
  - { name: "a", kernel: "PassThrough", executor: "host_a", input_ports: ["in_a"], output_ports: ["out_a"] }
  - { name: "b", kernel: "PassThrough", executor: "host_b", input_ports: ["in_b"], output_ports: ["out_b"] }
input_ports: ["in_a", "in_b"]
output_ports: ["out_a", "out_b"]
"#,
    )
    .unwrap();
    let out_b = graph.add_poller("out_b").unwrap();
    graph.start().unwrap();
    for i in 0..10i32 {
        graph
            .input("in_a")
            .unwrap()
            .send(Packet::new(i).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph
        .input("in_b")
        .unwrap()
        .send(Packet::new(99).at(Timestamp(0)))
        .unwrap();

    assert!(graph.pump_step(), "first delegated executor should advance");
    assert!(
        graph.pump_step(),
        "second delegated executor should not starve"
    );
    assert_eq!(
        out_b
            .try_next()
            .and_then(|packet| packet.get::<i32>().copied()),
        Some(99),
        "round-robin pumping must reach host_b even while host_a remains busy"
    );
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
}

#[test]
fn delegating_execution_is_serial_across_host_threads() {
    init();
    DELEGATED_ACTIVE.store(0, Ordering::SeqCst);
    DELEGATED_MAX_ACTIVE.store(0, Ordering::SeqCst);
    let graph = Arc::new(
        Graph::from_yaml(
            r#"
executors:
  - { name: "host_a", type: "DelegatingExecutor" }
  - { name: "host_b", type: "DelegatingExecutor" }
nodes:
  - { name: "a", kernel: "DelegatedSlowPass", executor: "host_a", input_ports: ["in_a"], output_ports: ["out_a"] }
  - { name: "b", kernel: "DelegatedSlowPass", executor: "host_b", input_ports: ["in_b"], output_ports: ["out_b"] }
input_ports: ["in_a", "in_b"]
output_ports: ["out_a", "out_b"]
"#,
        )
        .unwrap(),
    );
    graph.add_poller("out_a").unwrap();
    graph.add_poller("out_b").unwrap();
    graph.start().unwrap();
    graph
        .input("in_a")
        .unwrap()
        .send(Packet::new(1).at(Timestamp(0)))
        .unwrap();
    graph
        .input("in_b")
        .unwrap()
        .send(Packet::new(2).at(Timestamp(0)))
        .unwrap();

    let barrier = Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let graph = graph.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                graph.pump_step()
            })
        })
        .collect();
    barrier.wait();
    for handle in handles {
        let _ = handle.join().unwrap();
    }
    while graph.pump_step() {}

    assert_eq!(
        DELEGATED_MAX_ACTIVE.load(Ordering::SeqCst),
        1,
        "one graph must never execute delegated kernels concurrently"
    );
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
}

#[test]
fn rust_wakeup_callback_rearms_after_pump_drain() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: pass, kernel: PassThrough, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let wakes = Arc::new(AtomicUsize::new(0));
    let seen = wakes.clone();
    graph.set_wakeup_callback(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    });
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::new(1).at(Timestamp(0))).unwrap();
    input.send(Packet::new(2).at(Timestamp(1))).unwrap();
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    while graph.pump_step() {}
    input.send(Packet::new(3).at(Timestamp(2))).unwrap();
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    graph.clear_wakeup_callback();
    graph.cancel();
    assert!(matches!(
        graph.wait_done_timeout(Duration::from_secs(1)),
        Err(lmflow::Error::Cancelled)
    ));
}

#[test]
fn rust_wakeup_callback_rearms_after_panic() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: pass, kernel: PassThrough, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let wakes = Arc::new(AtomicUsize::new(0));
    let seen = wakes.clone();
    graph.set_wakeup_callback(move || {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("first wakeup fails");
        }
    });
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::new(1).at(Timestamp(0))).unwrap();
    graph.cancel();
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    graph.clear_wakeup_callback();
    assert!(matches!(
        graph.wait_done_timeout(Duration::from_secs(1)),
        Err(lmflow::Error::Cancelled)
    ));
}

/// 默认池是多线程的,所以默认节点配 `max_in_flight > 1` 现在**合法**(ADR #29 新语义)。
/// 单核机器上默认池只有 1 个线程,那时该报错 —— 两种结果都对,别把机器规格写进断言。
#[test]
fn max_in_flight_on_default_pool_follows_its_thread_count() {
    init();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let built = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"], max_in_flight: 4 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    );
    if threads > 1 {
        assert!(
            built.is_ok(),
            "默认池有 {threads} 个线程,max_in_flight>1 应当合法:{:?}",
            built.err()
        );
    } else {
        let err = built.expect_err("单线程默认池下并行度恒为 1,必须报错");
        assert!(err.to_string().contains("more than one thread"), "{err}");
    }
}

/// `DelegatingExecutor` 不拥有线程,故线程相关字段配在它上面是**报错**而不是静默忽略。
#[test]
fn delegating_executor_rejects_thread_fields() {
    init();
    for field in ["num_threads: 2", "affinity: [0]", "priority: 10"] {
        let err = Graph::from_yaml(&format!(
            r#"
executors:
  - {{ name: "host", type: "DelegatingExecutor", {field} }}
nodes:
  - {{ name: "p", kernel: "PassThrough", executor: "host", input_ports: ["in"], output_ports: ["out"] }}
input_ports: ["in"]
output_ports: ["out"]
"#
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("owns no threads"),
            "`{field}` 配在委托执行器上必须报错,actual: {err}"
        );
    }
}

/// Source 可以与普通节点共用单线程池；协作等待应使用 `source_yield`。
#[test]
fn source_and_ordinary_node_may_share_single_thread_pool() {
    init();
    Graph::from_yaml(
        r#"
executors:
  - { name: "solo", type: "ThreadPoolExecutor", num_threads: 1 }
nodes:
  - { name: "src", kernel: "FiniteSource", executor: "solo", input_ports: [], output_ports: ["mid"] }
  - { name: "p", kernel: "PassThrough", executor: "solo", input_ports: ["mid"], output_ports: ["out"] }
output_ports: ["out"]
"#,
    )
    .expect("cooperative sources can share a single-thread pool with ordinary nodes");
}

/// 混合执行器:一部分节点在池里、一部分交还宿主线程 —— 最容易死锁的组合。
#[test]
fn mixed_executors_do_not_deadlock() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 3 }
  - { name: "host", type: "DelegatingExecutor" }
nodes:
  - { name: "a", kernel: "PassThrough", executor: "cpu", input_ports: ["in"], output_ports: ["m1"] }
  - { name: "b", kernel: "PassThrough", executor: "host", input_ports: ["m1"], output_ports: ["m2"] }
  - { name: "c", kernel: "PassThrough", executor: "cpu", input_ports: ["m2"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    // 逐个「送一个取一个」—— 每次都要跨越 池→宿主委托→池 三段
    for i in 0..50i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
        let p = poller
            .next_timeout(Duration::from_secs(10))
            .expect("should not time out")
            .expect("should have output");
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
  - { name: "sp", kernel: "PassThrough", executor: "cpu", input_ports: ["in"], output_ports: ["fan"] }
  - { name: "pa", kernel: "PassThrough", executor: "cpu", input_ports: ["fan"], output_ports: ["oa"] }
  - { name: "pb", kernel: "PassThrough", executor: "cpu", input_ports: ["fan"], output_ports: ["ob"] }
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

    let count = |p: &lmflow::Poller| {
        let mut n = 0;
        while p.try_next().is_some() {
            n += 1;
        }
        n
    };
    assert_eq!(count(&pa), N as usize, "branch a should receive all");
    assert_eq!(count(&pb), N as usize, "branch b should receive all");
}

// ---------------------------------------------------------------- 阻塞语义

/// `wait_until_idle` 必须真的等到默认线程池干完,不能提前返回。
#[test]
fn wait_until_idle_waits_for_default_pool() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
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
    assert!(graph.is_idle(), "must truly be idle after idle returns");

    let mut n = 0;
    while poller.try_next().is_some() {
        n += 1;
    }
    assert_eq!(n, 100, "all packets should be produced after idle");
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
  - { name: "p", kernel: "PassThrough", executor: "cpu", input_ports: ["in"], output_ports: ["out"] }
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
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "must not hang forever"
    );
    assert!(
        matches!(r, Ok(None)),
        "idle with no data should return Ok(None), actual {r:?}"
    );
}

/// 暂停后不再调度;恢复后暂停期间到达的包必须被处理(不能一直躺着)。
#[test]
fn pause_and_resume() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
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
    assert!(
        poller.try_next().is_none(),
        "no output should occur while paused"
    );
    assert_eq!(
        graph.queue_depth("in"),
        Some(10),
        "packets should be backlogged"
    );

    graph.resume();
    assert!(!graph.is_paused());
    graph
        .wait_until_idle_timeout(Duration::from_secs(30))
        .unwrap();
    let mut n = 0;
    while poller.try_next().is_some() {
        n += 1;
    }
    assert_eq!(
        n, 10,
        "after resume, all packets backlogged while paused must be processed"
    );

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
nodes:
  - { name: "s", kernel: "Sink", input_ports: ["in"], output_ports: [] }
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
            "round {round}: Close must be called exactly once"
        );
        assert_eq!(
            graph.counter_value("sink.packets"),
            30,
            "round {round}: every packet should be processed once"
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
nodes:
  - { name: "sp", kernel: "PassThrough", input_ports: ["in"], output_ports: ["fan"] }
  - { name: "pa", kernel: "PassThrough", input_ports: ["fan"], output_ports: ["oa"] }
  - { name: "pb", kernel: "Sink", input_ports: ["fan"], output_ports: [] }
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
        "must not leak or double-free under concurrency + fanout"
    );
}

/// 多个宿主线程同时送包(引擎必须是线程安全的)。
#[test]
fn multiple_host_threads_sending() {
    init();
    let graph = std::sync::Arc::new(
        Graph::from_yaml(
            r#"
nodes:
  - { name: "a", kernel: "PassThrough", input_ports: ["in1"], output_ports: ["o1"] }
  - { name: "b", kernel: "PassThrough", input_ports: ["in2"], output_ports: ["o2"] }
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

    let count = |p: &lmflow::Poller| {
        let mut n = 0;
        while p.try_next().is_some() {
            n += 1;
        }
        n
    };
    assert_eq!(count(&p1), 100);
    assert_eq!(count(&p2), 100);
}

/// 多个宿主线程同时往**同一个**输入口送包 —— 比往不同口更难:`last_sent` 单调检查、
/// 同一条边的入队、全局水位记账都被并发争用。引擎必须线程安全:不崩、无竞态(TSan)、
/// 不泄漏,且每个**被接受**的包恰好投递一次。时间戳交错必然触发一些非单调拒绝,
/// 被拒的包也必须释放。
#[test]
fn concurrent_send_to_same_port_is_safe() {
    let _acct = ACCOUNTING.lock().unwrap_or_else(|e| e.into_inner());
    let base = ALIVE.load(Ordering::SeqCst);
    let accepted = std::sync::Arc::new(AtomicUsize::new(0));
    {
        init();
        let graph = std::sync::Arc::new(
            Graph::from_yaml(
                r#"
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
            )
            .unwrap(),
        );
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();

        let mut handles = Vec::new();
        for t in 0..4i64 {
            let g = graph.clone();
            let acc = accepted.clone();
            handles.push(std::thread::spawn(move || {
                let input = g.input("in").unwrap();
                // 4 个线程的时间戳区间交错(t, t+4, t+8, ...),到达顺序不定 → 必有非单调被拒
                for k in 0..50i64 {
                    let ts = k * 4 + t;
                    if input.send(tracked_packet(ts as i32, ts)).is_ok() {
                        acc.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        graph.close_all_inputs();
        graph.wait_done_timeout(Duration::from_secs(60)).unwrap();

        let mut delivered = 0usize;
        while poller.try_next().is_some() {
            delivered += 1;
        }
        assert_eq!(
            delivered,
            accepted.load(Ordering::SeqCst),
            "every accepted packet must be delivered exactly once (no loss, no dup)"
        );
    }
    assert_eq!(
        ALIVE.load(Ordering::SeqCst),
        base,
        "concurrent same-port sends must not leak (including non-monotonically-rejected packets)"
    );
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
nodes:
  - { name: "p", kernel: "PassThrough", input_ports: ["in"], output_ports: ["out"] }
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
        "teardown must still release all in-flight payloads"
    );
}
