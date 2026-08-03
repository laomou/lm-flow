//! lmflow 吞吐基准(Criterion)。三组量化引擎三条热路径:
//!   scheduling —— 吞吐 vs 管线深度(1/4/16 级 PassThrough)+ 单主线程 vs 4 线程池
//!   queue      —— 每口 `Mutex<VecDeque>` 的纯入队率,对照端到端(入队+出队+派发)率
//!   crossing   —— 每包 Rust→C++ FFI 往返:量化「跨界零拷贝」(I64 ≈ 大 buffer),
//!                 对照 InvertKernel 逐字节改写(O(payload))
//!
//! 看点:跨界按引用传(ffi.rs `borrow_packet` 只拷指针)—— PassThrough 转发 I64 与大
//! buffer 吞吐基本相同;payload 相关成本只在读写字节的算子(InvertKernel)才显现。
//!
//! 跑:在 `lmflow/core` 下 `cargo bench`;报告在 `target/criterion/`。

use std::cell::Cell;
use std::hint::black_box;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{
    criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion, Throughput,
};
use lmflow::{BufferData, Builtin, Graph, Packet, Timestamp};

/// 每次 Criterion 迭代喂入并排空的包数(吞吐以此为单位)。
const BATCH: u64 = 256;
/// dtype 常量(见 `src/packet.rs` 的 `mod dtype`):U8 = 0。
const DT_U8: i32 = 0;

fn init() {
    lmflow::register_builtin_kernels();
}

/// U8 buffer 包(InvertKernel 要求 dtype U8 + ndim ≥ 2)。
fn buf_u8(shape: &[i64]) -> Packet {
    Packet::from_builtin(Builtin::Buffer(BufferData::new(shape, DT_U8).unwrap()))
}

/// K 级 PassThrough 直通链的 YAML;`pool>0` 挂线程池,否则主线程执行器。
/// 名字相连即成边(n{i} 输出 e{i} = n{i+1} 输入 e{i})。`max_queue_size` 调大,批量喂入不撞软水位。
fn chain_yaml(depth: usize, pool: usize) -> String {
    let mut s = String::from("max_queue_size: 1000000\n");
    if pool > 0 {
        s += &format!(
            "executors:\n  - {{ name: \"cpu\", type: \"ThreadPoolExecutor\", num_threads: {pool} }}\n"
        );
    }
    s += "nodes:\n";
    for i in 0..depth {
        let inp = if i == 0 {
            "in".to_string()
        } else {
            format!("e{}", i - 1)
        };
        let out = if i == depth - 1 {
            "out".to_string()
        } else {
            format!("e{i}")
        };
        let exec = if pool > 0 { ", executor: \"cpu\"" } else { "" };
        s += &format!(
            "  - {{ name: \"n{i}\", kernel: \"PassThroughKernel\", input_ports: [\"{inp}\"], output_ports: [\"{out}\"]{exec} }}\n"
        );
    }
    s += "input_ports: [\"in\"]\noutput_ports: [\"out\"]\n";
    s
}

/// 单节点图:in -> <kernel> -> out(主线程执行器)。
fn single_yaml(kernel: &str) -> String {
    format!(
        "max_queue_size: 1000000\n\
         nodes:\n  - {{ name: \"n\", kernel: \"{kernel}\", input_ports: [\"in\"], output_ports: [\"out\"] }}\n\
         input_ports: [\"in\"]\noutput_ports: [\"out\"]\n"
    )
}

// ============================ scheduling ============================

fn bench_scheduling(c: &mut Criterion) {
    init();
    let mut g = c.benchmark_group("scheduling");
    g.throughput(Throughput::Elements(BATCH));

    // 单主线程:喂一取一(next() 会 pump 主线程任务,顺带跑完整条链)。
    for depth in [1usize, 4, 16] {
        let graph = Graph::from_yaml(&chain_yaml(depth, 0)).unwrap();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        let ts = Cell::new(0i64);
        g.bench_with_input(
            BenchmarkId::new("main_thread/depth", depth),
            &depth,
            |b, _| {
                b.iter(|| {
                    for _ in 0..BATCH {
                        let t = ts.get();
                        ts.set(t + 1);
                        input.send(Packet::from_i64(0).at(Timestamp(t))).unwrap();
                        black_box(poller.next());
                    }
                });
            },
        );
        // graph 在此 drop —— 计时闭包之外。
    }

    // 4 线程池:喂一批 -> wait_until_idle -> 排空(展示流水线并行,depth 越深越明显)。
    for depth in [1usize, 4, 16] {
        let graph = Graph::from_yaml(&chain_yaml(depth, 4)).unwrap();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        let ts = Cell::new(0i64);
        g.bench_with_input(BenchmarkId::new("pool4/depth", depth), &depth, |b, _| {
            b.iter(|| {
                for _ in 0..BATCH {
                    let t = ts.get();
                    ts.set(t + 1);
                    input.send(Packet::from_i64(0).at(Timestamp(t))).unwrap();
                }
                graph
                    .wait_until_idle_timeout(Duration::from_secs(30))
                    .unwrap();
                while let Some(p) = poller.try_next() {
                    black_box(p);
                }
            });
        });
        // graph 在此 drop(join 线程池)—— 计时闭包之外。
    }
    g.finish();
}

// ============================ queue ============================

fn bench_queue(c: &mut Criterion) {
    init();
    let mut g = c.benchmark_group("queue");
    g.throughput(Throughput::Elements(BATCH));

    // 纯入队:paused 图 send N(不派发)。iter_batched 每批新图,避免队列跨迭代堆积。
    g.bench_function("enqueue_paused", |b| {
        b.iter_batched(
            || {
                let graph = Graph::from_yaml(&single_yaml("PassThroughKernel")).unwrap();
                graph.start().unwrap();
                graph.pause();
                graph
            },
            |graph| {
                let input = graph.input("in").unwrap();
                for i in 0..BATCH {
                    input
                        .send(Packet::from_i64(0).at(Timestamp(i as i64)))
                        .unwrap();
                }
                black_box(&graph);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // 端到端(入队+出队+派发+FFI):与上面的差 ≈ 调度 + 跨界一跳。
    let graph = Graph::from_yaml(&single_yaml("PassThroughKernel")).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    let ts = Cell::new(0i64);
    g.bench_function("end_to_end", |b| {
        b.iter(|| {
            for _ in 0..BATCH {
                let t = ts.get();
                ts.set(t + 1);
                input.send(Packet::from_i64(0).at(Timestamp(t))).unwrap();
                black_box(poller.next());
            }
        });
    });
    g.finish();
}

// ============================ crossing (FFI) ============================

/// 单节点图上喂一取一;`template` 预建一次,每包 `clone().at(t)`(clone 只增 Arc 计数,
/// 不重新分配)—— 把分配成本挪出计时,量到的是纯跨界 + 算子处理。
fn drive_single(g: &mut BenchmarkGroup<'_, WallTime>, id: &str, kernel: &str, template: Packet) {
    let graph = Graph::from_yaml(&single_yaml(kernel)).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    let ts = Cell::new(0i64);
    g.bench_function(id, |b| {
        b.iter(|| {
            for _ in 0..BATCH {
                let t = ts.get();
                ts.set(t + 1);
                input.send(template.clone().at(Timestamp(t))).unwrap();
                black_box(poller.next());
            }
        });
    });
}

fn bench_crossing(c: &mut Criterion) {
    init();
    let mut g = c.benchmark_group("crossing");
    g.throughput(Throughput::Elements(BATCH));

    // 零拷贝坐实:PassThrough 转发 I64 vs ~768KB buffer —— 吞吐应基本相同。
    drive_single(
        &mut g,
        "passthrough/i64",
        "PassThroughKernel",
        Packet::from_i64(0),
    );
    drive_single(
        &mut g,
        "passthrough/buffer_768k",
        "PassThroughKernel",
        buf_u8(&[512, 512, 3]),
    );

    // 对照:InvertKernel 逐字节改写(共享 payload 触发 CoW 复制)—— 吞吐随 payload 明显下降。
    drive_single(
        &mut g,
        "invert/buffer_256b",
        "InvertKernel",
        buf_u8(&[16, 16]),
    );
    drive_single(
        &mut g,
        "invert/buffer_768k",
        "InvertKernel",
        buf_u8(&[512, 512, 3]),
    );

    g.finish();
}

criterion_group!(benches, bench_scheduling, bench_queue, bench_crossing);
criterion_main!(benches);
