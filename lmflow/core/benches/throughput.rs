//! Pure-Rust end-to-end throughput benchmarks.
//!
//! These benchmarks include graph input, scheduling, a private Rust pass-through
//! kernel, and Poller output. They complement `dispatch.rs`, which deliberately
//! removes Poller overhead to isolate scheduler dispatch cost.
//!
//! Run from `lmflow/core`:
//!
//! ```sh
//! cargo bench --bench throughput
//! ```

mod common;

use std::cell::Cell;
use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lmflow::{BufferData, Builtin, Graph, Packet, Timestamp};

const BATCH: u64 = 256;
const DT_U8: i32 = 0;

fn chain_yaml(depth: usize, threads: usize) -> String {
    let mut yaml = String::from("max_queue_size: 1000000\n");
    if threads == 0 {
        yaml.push_str("executors:\n  - { name: \"host\", type: \"DelegatingExecutor\" }\n");
    } else {
        yaml.push_str(&format!(
            "executors:\n  - {{ name: \"cpu\", type: \"ThreadPoolExecutor\", num_threads: {threads} }}\n"
        ));
    }
    yaml.push_str("nodes:\n");
    let executor = if threads == 0 { "host" } else { "cpu" };
    for index in 0..depth {
        let input = if index == 0 {
            "in".to_string()
        } else {
            format!("edge{}", index - 1)
        };
        let output = if index + 1 == depth {
            "out".to_string()
        } else {
            format!("edge{index}")
        };
        yaml.push_str(&format!(
            "  - {{ name: \"node{index}\", kernel: \"BenchPassThrough\", executor: \"{executor}\", input_ports: [\"{input}\"], output_ports: [\"{output}\"] }}\n"
        ));
    }
    yaml.push_str("input_ports: [\"in\"]\noutput_ports: [\"out\"]\n");
    yaml
}

fn bench_round_trip(c: &mut Criterion) {
    common::register_bench_kernels();
    let mut group = c.benchmark_group("rust_end_to_end");
    group.throughput(Throughput::Elements(BATCH));

    for depth in [1usize, 4, 16] {
        let graph = Graph::from_yaml(&chain_yaml(depth, 0)).unwrap();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        let timestamp = Cell::new(0i64);

        group.bench_with_input(
            BenchmarkId::new("delegating_i64_depth", depth),
            &depth,
            |benchmark, _| {
                benchmark.iter(|| {
                    for _ in 0..BATCH {
                        let current = timestamp.get();
                        timestamp.set(current + 1);
                        input
                            .send(Packet::from_i64(0).at(Timestamp(current)))
                            .unwrap();
                        black_box(poller.next().unwrap());
                    }
                });
            },
        );
    }
    group.finish();
}

fn bench_thread_pool(c: &mut Criterion) {
    common::register_bench_kernels();
    let mut group = c.benchmark_group("rust_end_to_end");
    group.throughput(Throughput::Elements(BATCH));

    for depth in [1usize, 4, 16] {
        let graph = Graph::from_yaml(&chain_yaml(depth, 4)).unwrap();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        let timestamp = Cell::new(0i64);

        group.bench_with_input(
            BenchmarkId::new("pool4_i64_depth", depth),
            &depth,
            |benchmark, _| {
                benchmark.iter(|| {
                    for _ in 0..BATCH {
                        let current = timestamp.get();
                        timestamp.set(current + 1);
                        input
                            .send(Packet::from_i64(0).at(Timestamp(current)))
                            .unwrap();
                    }
                    graph
                        .wait_until_idle_timeout(Duration::from_secs(30))
                        .unwrap();
                    for _ in 0..BATCH {
                        black_box(poller.next().unwrap());
                    }
                });
            },
        );
    }
    group.finish();
}

fn bench_payload_independence(c: &mut Criterion) {
    common::register_bench_kernels();
    let mut group = c.benchmark_group("rust_zero_copy");
    group.throughput(Throughput::Elements(BATCH));

    let large = Packet::from_builtin(Builtin::Buffer(
        BufferData::new(&[512, 512, 3], DT_U8).unwrap(),
    ));
    for (name, template) in [("i64", Packet::from_i64(0)), ("buffer_768k", large)] {
        let graph = Graph::from_yaml(&chain_yaml(1, 0)).unwrap();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        let timestamp = Cell::new(0i64);

        group.bench_function(name, |benchmark| {
            benchmark.iter(|| {
                for _ in 0..BATCH {
                    let current = timestamp.get();
                    timestamp.set(current + 1);
                    input.send(template.clone().at(Timestamp(current))).unwrap();
                    black_box(poller.next().unwrap());
                }
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_round_trip,
    bench_thread_pool,
    bench_payload_independence
);
criterion_main!(benches);
