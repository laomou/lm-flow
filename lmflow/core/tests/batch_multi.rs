//! `batch` 策略的**多输入口**验收(纯 Rust 算子,故两种 feature 配置下都跑)。
//!
//! 「一批」的定义是 `size` 个**对齐元组** —— 对齐规则与 `sync` 完全同源,而不是
//! 「各口各自数够 `size` 个」。后者会把 0 号口的第 k 个与 1 号口的第 k 个配成一对,
//! 而它们未必是同一帧;那是**静默的错误配对**,本项目不接受。
//!
//! 因此本文件最核心的一条是 `sparse_port_yields_unequal_batch_sizes`:两个口的包数
//! 不同时,各口取数**就该不同**,而不是硬凑成一样长。

use std::time::Duration;

use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet, State, Timestamp};

/// 把「本次每个口各收到哪些时间戳」编码成字符串产出 —— 这样断言直接落在对齐语义上,
/// 而不是落在某个聚合结果上(聚合会把「配错了对」这件事平均掉、看不出来)。
#[derive(Default)]
struct BatchProbe;

impl Kernel for BatchProbe {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let mut parts = Vec::new();
        for port in 0..cc.num_inputs() {
            let tss: Vec<String> = (0..cc.input_count(port))
                .filter_map(|k| cc.input_at(port, k).map(|p| p.timestamp().0.to_string()))
                .collect();
            parts.push(format!("p{port}=[{}]", tss.join(",")));
        }
        cc.emit(0, Packet::new(parts.join(" ")))
    }
}

fn init() {
    let _ = register_kernel::<BatchProbe>("BatchProbe");
}

fn drain(poller: &lmflow::Poller) -> Vec<String> {
    std::iter::from_fn(|| poller.next())
        .map(|p| p.get::<String>().cloned().unwrap_or_default())
        .collect()
}

fn two_port_graph(capacity: usize, executor: bool) -> String {
    let (execs, on) = if executor {
        (
            "executors:\n  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }\n",
            "    executor: cpu\n",
        )
    } else {
        ("", "")
    };
    format!(
        r#"{execs}nodes:
  - name: b
    kernel: BatchProbe
{on}    input_ports: ["x", "y"]
    output_ports: ["out"]
    input_policy: {{ type: batch, capacity: {capacity} }}
input_ports: ["x", "y"]
output_ports: ["out"]
"#
    )
}

/// 两个口时间戳完全一致:每批就是 `size` 个对齐元组,两口取数相同。
#[test]
fn aligned_streams_batch_together() {
    init();
    let graph = Graph::from_yaml(&two_port_graph(3, false)).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (x, y) = (graph.input("x").unwrap(), graph.input("y").unwrap());

    for i in 0..6i64 {
        x.send(Packet::new(i).at(Timestamp(i))).unwrap();
        y.send(Packet::new(i * 10).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(graph.state(), State::Terminated);

    assert_eq!(
        drain(&poller),
        vec!["p0=[0,1,2] p1=[0,1,2]", "p0=[3,4,5] p1=[3,4,5]"],
        "两口时间戳一致 → 每批 3 个对齐元组"
    );
}

/// **本文件的核心**:某口稀疏时,各口取数**就该不同**。
///
/// x: ts 0,1,2,3   y: ts 0,2(缺 1 和 3)
/// 对齐逐轮取全局最小:轮1 @0(两口都取)、轮2 @1(只 x 有)→ 第一批 x 取 2 个、y 取 1 个。
/// 若实现改成「各口都凑 2 个」,y 就会把 ts=2 的包提前配进第一批 —— 那是错帧配对。
#[test]
fn sparse_port_yields_unequal_batch_sizes() {
    init();
    let graph = Graph::from_yaml(&two_port_graph(2, false)).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (x, y) = (graph.input("x").unwrap(), graph.input("y").unwrap());

    for t in [0i64, 1, 2, 3] {
        x.send(Packet::new(t).at(Timestamp(t))).unwrap();
    }
    for t in [0i64, 2] {
        y.send(Packet::new(t).at(Timestamp(t))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    let got = drain(&poller);
    assert_eq!(
        got,
        vec!["p0=[0,1] p1=[0]", "p0=[2,3] p1=[2]"],
        "各口取数按对齐结果走,不硬凑成等长(否则就是错帧配对)"
    );
}

/// 不足一批时:只有**所有正向口都关了**才刷余量;数据不丢。
#[test]
fn partial_batch_flushed_only_after_close() {
    init();
    let graph = Graph::from_yaml(&two_port_graph(4, false)).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (x, y) = (graph.input("x").unwrap(), graph.input("y").unwrap());

    // 只喂 2 个元组,不足 capacity=4
    for i in 0..2i64 {
        x.send(Packet::new(i).at(Timestamp(i))).unwrap();
        y.send(Packet::new(i).at(Timestamp(i))).unwrap();
    }
    // 还没关流 → 不该有任何产出(过早切批也是一种语义偏差)
    assert!(poller.try_next().is_none(), "不足一批且未关流时不该触发");

    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(
        drain(&poller),
        vec!["p0=[0,1] p1=[0,1]"],
        "关流后把不足一批的余量刷出,不丢数据"
    );
}

/// 一个口有数据、另一个口既空又**未关闭**时,必须等 —— 不能拿单边数据凑批。
/// 这条守的是 `min_bound <= min_packet` 那个判断:空口还可能送来更早的包。
#[test]
fn does_not_fire_while_one_port_is_starved_and_open() {
    init();
    let graph = Graph::from_yaml(&two_port_graph(2, false)).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (x, y) = (graph.input("x").unwrap(), graph.input("y").unwrap());

    for i in 0..4i64 {
        x.send(Packet::new(i).at(Timestamp(i))).unwrap();
    }
    std::thread::sleep(Duration::from_millis(30));
    assert!(
        poller.try_next().is_none(),
        "y 口空且未关闭 → 它还可能送来更早的包,不能先把 x 的批定下来"
    );

    // y 一到齐就该能凑出批
    for i in 0..2i64 {
        y.send(Packet::new(i).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    let got = drain(&poller);
    assert_eq!(got.first().map(String::as_str), Some("p0=[0,1] p1=[0,1]"));
}

/// 关掉一个口之后,另一个口可以独自继续成批 —— 关闭的口 bound 变 done,不再阻塞对齐。
#[test]
fn closing_one_port_unblocks_the_other() {
    init();
    let graph = Graph::from_yaml(&two_port_graph(2, false)).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (x, y) = (graph.input("x").unwrap(), graph.input("y").unwrap());

    for i in 0..4i64 {
        x.send(Packet::new(i).at(Timestamp(i))).unwrap();
    }
    drop(y);
    graph.close_input("y").unwrap();
    graph.close_input("x").unwrap();
    graph.wait_done().unwrap();

    assert_eq!(
        drain(&poller),
        vec!["p0=[0,1] p1=[]", "p0=[2,3] p1=[]"],
        "y 关闭后 x 独自成批;y 这一侧本批为空"
    );
}

/// 多口 batch 跑在线程池上:worker 做 try_claim 的整批弹包、主线程 pump ——
/// 并发触达调度热路径(TSan 门禁覆盖这条)。
#[test]
fn multi_port_batch_on_thread_pool() {
    init();
    let graph = Graph::from_yaml(&two_port_graph(4, true)).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (x, y) = (graph.input("x").unwrap(), graph.input("y").unwrap());

    for i in 0..8i64 {
        x.send(Packet::new(i).at(Timestamp(i))).unwrap();
        y.send(Packet::new(i).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph
        .wait_done_timeout(Duration::from_secs(30))
        .expect("should terminate");

    assert_eq!(
        drain(&poller),
        vec!["p0=[0,1,2,3] p1=[0,1,2,3]", "p0=[4,5,6,7] p1=[4,5,6,7]"],
        "线程池下批的划分与顺序都不该变"
    );
}

/// 单输入口的 batch 行为不因多口支持而改变(回归)。
#[test]
fn single_port_batch_unchanged() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: b
    kernel: BatchProbe
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: batch, capacity: 3 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..7i64 {
        input.send(Packet::new(i).at(Timestamp(i))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done().unwrap();

    assert_eq!(
        drain(&poller),
        vec!["p0=[0,1,2]", "p0=[3,4,5]", "p0=[6]"],
        "满批 ×2 + 关流刷余批"
    );
}
