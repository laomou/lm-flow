//! hello_world —— 最小可运行示例:两级直通管线。
//!
//! 拓扑:input1 → node1(PassThrough) → input2 → node2(PassThrough) → output2
//! 使用 core 自带的纯 Rust `PassThrough` 算子，一条 `cargo run` 即可跑通。
//! 对应 C++ 宿主版本见 examples/cpp/hello_world。

use lmflow::{Graph, Packet, Timestamp};

// 两个节点都未指定 executor,故都归**默认执行器** —— 一个按 CPU 核数开线程的线程池,
// 引擎自动创建,不必写 executors 块。想要零并发、顺序确定(便于断点调试)就把默认
// 自己声明一个委托执行器、把节点指过去:
// executors: [{ name: "host", type: "DelegatingExecutor" }] + 节点上写 executor: "host"。
const CONFIG: &str = r#"
nodes:
  - name: "node1"
    kernel: "PassThrough"
    input_ports: ["input1"]
    output_ports: ["input2"]
  - name: "node2"
    kernel: "PassThrough"
    input_ports: ["input2"]
    output_ports: ["output2"]
input_ports: ["input1"]
output_ports: ["output2"]
"#;

fn main() -> lmflow::Result<()> {
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
