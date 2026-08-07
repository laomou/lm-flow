//! Route predicate and packet metadata benchmarks.
//!
//! Run from `lmflow/core`:
//!
//! ```sh
//! cargo bench --bench route_metadata
//! ```

use std::cell::Cell;
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lmflow::{Graph, Packet, Timestamp};

const BATCH: u64 = 256;

fn route_yaml(mode: &str, rules: usize) -> String {
    let mut yaml = format!(
        "nodes:\n  - name: router\n    type: route\n    input_ports: [in]\n    output_ports: [out]\n    mode: {mode}\n    unmatched: out\n    routes:\n"
    );
    for index in 0..rules {
        yaml.push_str(&format!(
            "      - {{ to: out, when: {{ metadata: score, op: gte, value: {} }} }}\n",
            index + 1
        ));
    }
    yaml.push_str("input_ports: [in]\noutput_ports: [out]\n");
    yaml
}

fn bench_metadata(c: &mut Criterion) {
    let mut group = c.benchmark_group("packet_metadata");
    let template = Packet::from_i64(7)
        .with_metadata("confidence", 0.9)
        .with_metadata("category", "person");

    group.bench_function("lookup_two_keys", |benchmark| {
        benchmark.iter(|| {
            black_box(template.metadata_value("confidence"));
            black_box(template.metadata_value("category"));
        });
    });
    group.bench_function("clone_then_set_cow", |benchmark| {
        benchmark.iter(|| {
            let mut packet = template.clone();
            packet.set_metadata("confidence", 0.8);
            black_box(packet);
        });
    });
    group.bench_function("list_keys", |benchmark| {
        benchmark.iter(|| black_box(template.metadata_keys().count()));
    });
    group.finish();
}

fn bench_route(c: &mut Criterion) {
    let mut group = c.benchmark_group("route_end_to_end");
    group.throughput(Throughput::Elements(BATCH));

    for mode in ["first", "all"] {
        for rules in [1usize, 4, 16] {
            let graph = Graph::from_yaml(&route_yaml(mode, rules)).unwrap();
            let poller = graph.add_poller("out").unwrap();
            graph.start().unwrap();
            let input = graph.input("in").unwrap();
            let timestamp = Cell::new(0i64);

            group.bench_with_input(BenchmarkId::new(mode, rules), &rules, |benchmark, _| {
                benchmark.iter(|| {
                    for _ in 0..BATCH {
                        let current = timestamp.get();
                        timestamp.set(current + 1);
                        input
                            .send(
                                Packet::from_i64(7)
                                    .at(Timestamp(current))
                                    .with_metadata("score", rules as i64),
                            )
                            .unwrap();
                        let output_count = if mode == "all" { rules } else { 1 };
                        for _ in 0..output_count {
                            black_box(poller.next().unwrap());
                        }
                    }
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_metadata, bench_route);
criterion_main!(benches);
