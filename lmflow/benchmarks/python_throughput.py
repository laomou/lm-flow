#!/usr/bin/env python3
"""Python binding throughput smoke benchmark.

Run against an installed wheel or an editable/local installation:

    python lmflow/benchmarks/python_throughput.py
    python lmflow/benchmarks/python_throughput.py --iterations 10000

This intentionally measures the public Python API, including conversion between
Python values/NumPy arrays and engine packets. It is not a replacement for the
lower-noise Rust Criterion or native C API benchmarks.
"""

from __future__ import annotations

import argparse
import time

import numpy as np

import lmflow


def graph_yaml(kernel: str) -> str:
    return f"""
max_queue_size: 1000000
executors:
  - {{ name: host, type: DelegatingExecutor }}
nodes:
  - {{ name: node, kernel: {kernel}, executor: host, input_ports: [in], output_ports: [out] }}
input_ports: [in]
output_ports: [out]
"""


def benchmark(name: str, kernel: str, value: object, iterations: int) -> None:
    with lmflow.Graph.from_yaml(graph_yaml(kernel)) as graph:
        output = graph.add_poller("out")
        input_port = graph.input("in")
        graph.start()

        warmup = iterations // 10 + 1
        for timestamp in range(warmup):
            input_port.send(value, timestamp)
            output.next()

        started = time.perf_counter_ns()
        for timestamp in range(iterations):
            input_port.send(value, warmup + timestamp)
            output.next()
        elapsed_ns = time.perf_counter_ns() - started

    packets_per_second = iterations * 1e9 / elapsed_ns
    nanoseconds_per_packet = elapsed_ns / iterations
    print(
        f"{name:<38}{packets_per_second:>14.1f} pkt/s"
        f"{nanoseconds_per_packet:>14.1f} ns/pkt"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--iterations", type=int, default=5_000)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be positive")

    lmflow.register_builtin_kernels()
    benchmark("python/pass_through/i64", "PassThroughKernel", 0, args.iterations)

    large = np.zeros((512, 512, 3), dtype=np.uint8)
    benchmark(
        "python/pass_through/numpy_768k",
        "PassThroughKernel",
        large,
        max(1, args.iterations // 100),
    )
    benchmark(
        "python/invert/numpy_768k",
        "InvertKernel",
        large,
        max(1, args.iterations // 100),
    )


if __name__ == "__main__":
    main()
