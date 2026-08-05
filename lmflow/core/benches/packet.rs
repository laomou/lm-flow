//! Pure-Rust Packet reference-counting and copy-on-write benchmarks.
//!
//! Run from `lmflow/core`:
//!
//! ```sh
//! cargo bench --bench packet
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use lmflow::{BufferData, Builtin, Packet};

const DT_U8: i32 = 0;

fn buffer_packet(bytes: usize) -> Packet {
    Packet::from_builtin(Builtin::Buffer(
        BufferData::new(&[bytes as i64], DT_U8).unwrap(),
    ))
}

fn bench_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("packet_clone");
    group.throughput(Throughput::Elements(1));

    for bytes in [8usize, 4 * 1024, 768 * 1024] {
        let packet = buffer_packet(bytes);
        group.bench_with_input(
            BenchmarkId::from_parameter(bytes),
            &bytes,
            |benchmark, _| {
                benchmark.iter(|| black_box(packet.clone()));
            },
        );
    }
    group.finish();
}

fn bench_cow(c: &mut Criterion) {
    let mut group = c.benchmark_group("packet_cow");

    for bytes in [256usize, 4 * 1024, 768 * 1024] {
        group.throughput(Throughput::Bytes(bytes as u64));

        group.bench_with_input(
            BenchmarkId::new("exclusive", bytes),
            &bytes,
            |benchmark, &size| {
                benchmark.iter_batched(
                    || buffer_packet(size),
                    |mut packet| {
                        black_box(packet.make_mutable_builtin().unwrap());
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );

        group.bench_with_input(
            BenchmarkId::new("shared_copy", bytes),
            &bytes,
            |benchmark, &size| {
                benchmark.iter_batched(
                    || {
                        let packet = buffer_packet(size);
                        let shared = packet.clone();
                        (packet, shared)
                    },
                    |(mut packet, shared)| {
                        black_box(packet.make_mutable_builtin().unwrap());
                        black_box(shared);
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_clone, bench_cow);
criterion_main!(benches);
