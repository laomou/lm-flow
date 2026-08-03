//! **引擎派发**基准(Criterion)—— 把「引擎自己的开销」从「宿主取输出的开销」里剥出来。
//!
//! # 为什么单独一组
//!
//! `throughput.rs` 里的 `queue/end_to_end` 与 `crossing/*` 都是 `send` 一个、`poller.next()`
//! 取一个的**往返**。而默认执行器就是宿主主线程(ADR #16),`next()` 本身就在
//! **pump 任务**(design §7.9)—— 于是「驱动图」和「取输出」被算进了同一个数字里,
//! 还额外含 poller 队列锁与 condvar。那个数字是**宿主往返延迟**,不是引擎吞吐。
//!
//! 这里换一种口径:图以 **`Sink` 结尾、没有图输出口**,喂满一批后用
//! `wait_until_idle` 排干。**全程不涉及 poller**,量到的是
//! 入队 → 就绪判定 → 派发 → vtable 调用 → 算子体 → 汇点。
//! `wait_until_idle` 每批只调一次(BATCH=256),摊到每包近乎为零。
//!
//! # 怎么读这些数
//!
//! **最可信的一个数**:`depth16` 与 `depth1` 之差 ÷ 15 = **每跳边际派发成本**
//! (节点之间多转发一包要多少)。取差值把「建图/喂入/收尾」等固定成本消掉了,
//! 所以它比任何单点绝对值都稳。
//!
//! ⚠ **不要**拿本组的 `depth1` 直接减 `throughput.rs::queue/end_to_end` 去算「poller 的代价」:
//! 本组 `depth1` 是**两个节点**(1 个 PassThrough + 末端 Sink),而 `end_to_end` 是
//! 一个节点 + poller,节点数就不一样,减出来的数没有意义。要比就比每跳边际值。
//!
//! `main_thread` 与 `pool*` 之差 = 走线程池派发(投递 + 唤醒 + 跨线程 + activity 通知)
//! 相对主线程内联执行的差价。注意 `pool1` 是「付了跨线程成本却没有并行度」的最差组合,
//! `pool4` 则靠流水线并行(多跳同时在跑)把它赚回来 —— 深链上才体现。
//!
//! # 本机实测(2026-08-03,x86_64;换算成每包,BATCH=256)
//!
//! | 配置 | 每包 | 每跳边际 | 可信度 |
//! |---|---|---|---|
//! | `sink/main_thread` | 573 ns | **279 ns** | 区间 ±0.3%,可用于归因 |
//! | `sink/pool1` | 3973 ns | 2077 ns | 区间 ±0.5%,可用于归因 |
//! | `sink/pool4` | ~4000 ns | ~400 ns | **±13~25%,不可归因** |
//! | `enqueue_only_paused` | ~80~100 ns | — | 跑间漂移大,只作数量级参照 |
//!
//! **`pool4` 与 `enqueue_only_paused` 的波动大到不能用来归因代码改动**(前者是 4 线程抢
//! 2 节点的锁 + 线程落位靠运气;后者绝对值太小、易受机器负载影响)。判断某次改动有没有
//! 效果,请看 `main_thread` / `pool1`。
//!
//! 已经因此误读过两次,都是**复测才发现是噪声**:
//! * 一个纯删堆分配的改动在 `pool4` 上显示「回归 42%」,复测三次 880/830/945 µs;
//! * 一个只减少系统调用的改动让 `enqueue_only_paused`(根本不涉及该路径)显示「回归 9.5%」。
//!
//! ⚠ **本机的系统调用被放大约 5 倍**:`getpid()` 实测 329 ns(裸机通常 50~80 ns),
//! `Condvar::notify_all()` 即使无等待者也要 ~372 ns。因此任何「省掉系统调用」类改动
//! 在本机上的**相对收益会被放大** —— 判断真实收益要按「省了几次系统调用」而不是按本机百分比。
//! 本机的原语单价参照:Mutex lock+unlock 4.0 ns · 原子 RMW 1.8 ns ·
//! `Instant::now()`+`elapsed` 43 ns · 经 fn 指针的间接调用 0.5 ns。
//!
//! ⚠ 别把本组与 `crossing/*` 之差当成「FFI 开销」:Rust 算子与 C++ 算子**走的是同一套
//! 函数指针 vtable**(`kernel_api` 的蹦床同样是 `extern "C"`),差的是**算子体**本身,
//! 不是过不过边界。
//!
//! 本组只用引擎自带的 Rust 默认算子(`PassThrough` / `Sink`),故**不需要
//! `builtin-kernels` feature** —— 纯 Rust 配置下也能量引擎。
//!
//! 跑:在 `lmflow/core` 下 `cargo bench --bench dispatch`;报告在 `target/criterion/`。

use std::cell::Cell;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lmflow::{Graph, Packet, Timestamp};

/// 每次 Criterion 迭代喂入并排干的包数(吞吐以此为单位)。
const BATCH: u64 = 256;

/// `depth` 级 `PassThrough` 之后接一个 `Sink`,**没有图输出口**。
/// `pool > 0` 时全部节点挂线程池,否则走宿主主线程执行器。
/// `max_queue_size` 调大,批量喂入不撞全局软水位(那会转成输入口背压,污染测量)。
fn sink_chain_yaml(depth: usize, pool: usize) -> String {
    let mut s = String::from("max_queue_size: 1000000\n");
    if pool > 0 {
        s += &format!(
            "executors:\n  - {{ name: \"cpu\", type: \"ThreadPoolExecutor\", num_threads: {pool} }}\n"
        );
    }
    s += "nodes:\n";
    let exec = if pool > 0 { ", executor: \"cpu\"" } else { "" };
    for i in 0..depth {
        let inp = if i == 0 {
            "in".to_string()
        } else {
            format!("e{}", i - 1)
        };
        s += &format!(
            "  - {{ name: \"n{i}\", kernel: \"PassThrough\", input_ports: [\"{inp}\"], output_ports: [\"e{i}\"]{exec} }}\n"
        );
    }
    // 末端汇点:只消费、零输出口 —— 于是整张图没有图输出口,poller 完全不参与。
    s += &format!(
        "  - {{ name: \"sink\", kernel: \"Sink\", input_ports: [\"e{}\"], output_ports: []{exec} }}\n",
        depth - 1
    );
    s += "input_ports: [\"in\"]\noutput_ports: []\n";
    s
}

fn bench_dispatch(c: &mut Criterion) {
    let mut g = c.benchmark_group("dispatch");
    g.throughput(Throughput::Elements(BATCH));

    for (label, pool) in [("main_thread", 0usize), ("pool1", 1), ("pool4", 4)] {
        for depth in [1usize, 16] {
            let graph = Graph::from_yaml(&sink_chain_yaml(depth, pool)).unwrap();
            graph.start().unwrap();
            let input = graph.input("in").unwrap();
            let ts = Cell::new(0i64);
            g.bench_with_input(
                BenchmarkId::new(format!("sink/{label}/depth"), depth),
                &depth,
                |b, _| {
                    b.iter(|| {
                        for _ in 0..BATCH {
                            let t = ts.get();
                            ts.set(t + 1);
                            input.send(Packet::from_i64(0).at(Timestamp(t))).unwrap();
                        }
                        // 每批一次:摊到每包近乎为零,且**不经 poller**
                        graph
                            .wait_until_idle_timeout(Duration::from_secs(30))
                            .unwrap();
                    });
                },
            );
            // graph 在此 drop(join 线程池)—— 计时闭包之外。
        }
    }
    g.finish();
}

/// 只量「送进图输入口」这一步(图暂停,不派发)—— 作为上面各数字的下界参照。
/// 与 `throughput.rs::queue/enqueue_paused` 同口径,但这里用 Rust 默认算子,
/// 故纯 Rust 配置下也能跑。
fn bench_enqueue_only(c: &mut Criterion) {
    let mut g = c.benchmark_group("dispatch");
    g.throughput(Throughput::Elements(BATCH));
    g.bench_function("enqueue_only_paused", |b| {
        b.iter_batched(
            || {
                let graph = Graph::from_yaml(&sink_chain_yaml(1, 0)).unwrap();
                graph.start().unwrap();
                graph.pause(); // 只入队,不派发
                graph
            },
            |graph| {
                let input = graph.input("in").unwrap();
                for i in 0..BATCH {
                    input
                        .send(Packet::from_i64(0).at(Timestamp(i as i64)))
                        .unwrap();
                }
                graph
            },
            criterion::BatchSize::SmallInput,
        );
    });
    g.finish();
}

criterion_group!(benches, bench_enqueue_only, bench_dispatch);
criterion_main!(benches);
