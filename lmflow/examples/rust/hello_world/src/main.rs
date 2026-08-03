//! hello_world —— 最小可运行示例:两级直通管线。
//!
//! 拓扑:input1 → node1(PassThrough) → input2 → node2(PassThrough) → output2
//! 算子是 C++ 写的(经 lmflow crate 的 build.rs 用 cc 编译链入),引擎与 host 是 Rust ——
//! 一条 `cargo run` 即可跑通全链路。对应 C++ 版见 examples/cpp/hello_world。

use lmflow::{Graph, Packet, Timestamp};

// 两个节点都未指定 executor,故都跑在**主线程**上 —— 任务在 poller.next() /
// wait_done() 等阻塞调用期间被抽取执行。要并发就显式声明 executors 并给节点指定。
const CONFIG: &str = r#"
nodes:
  - name: "node1"
    kernel: "PassThroughKernel"
    input_ports: ["input1"]
    output_ports: ["input2"]
  - name: "node2"
    kernel: "PassThroughKernel"
    input_ports: ["input2"]
    output_ports: ["output2"]
input_ports: ["input1"]
output_ports: ["output2"]
"#;

fn main() -> lmflow::Result<()> {
    // C++ 算子的注册:静态初始化可能被链接器裁剪,故显式聚合注册一次(见设计文档 §9)。
    lmflow::register_builtin_kernels();

    let graph = Graph::from_yaml(CONFIG)?;
    let poller = graph.add_poller("output2")?;
    graph.start()?;

    // 句柄式输入:热路径免去每包按名字查表
    let input = graph.input("input1")?;

    for i in 0..10i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64)))?;

        // 灌一个、取一个(与 C++ 版同样的同步节奏)
        match poller.next() {
            Some(pkt) => println!("out: {} @ ts={}", pkt.get::<i32>().unwrap(), pkt.timestamp().0),
            None => break, // 图已结束
        }
    }

    graph.close_all_inputs();
    graph.wait_done()?; // Packet 全部 RAII 回收,无需手动 drop
    Ok(())
}
