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
import json
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


def benchmark(name: str, kernel: str, value: object, iterations: int) -> dict[str, object]:
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
    payload_bytes = int(value.nbytes) if isinstance(value, np.ndarray) else 0
    result: dict[str, object] = {
        "name": name,
        "iterations": iterations,
        "payload_bytes": payload_bytes,
        "packets_per_second": packets_per_second,
        "nanoseconds_per_packet": nanoseconds_per_packet,
    }
    if payload_bytes:
        result["mib_per_second"] = packets_per_second * payload_bytes / (1024 * 1024)
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--iterations", type=int, default=5_000)
    parser.add_argument("--json", action="store_true", help="emit one machine-readable JSON document")
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be positive")

    results = [benchmark("python/pass_through/i64", "PassThroughKernel", 0, args.iterations)]

    large = np.zeros((512, 512, 3), dtype=np.uint8)
    results.append(benchmark(
        "python/pass_through/numpy_768k",
        "PassThroughKernel",
        large,
        max(1, args.iterations // 100),
    ))
    results.append(benchmark(
        "python/invert/numpy_768k",
        "InvertKernel",
        large,
        max(1, args.iterations // 100),
    ))
    if args.json:
        print(json.dumps({"language": "python", "results": results}, separators=(",", ":")))
    else:
        for result in results:
            print(
                f"{result['name']:<38}{result['packets_per_second']:>14.1f} pkt/s"
                f"{result['nanoseconds_per_packet']:>14.1f} ns/pkt"
            )


if __name__ == "__main__":
    main()
