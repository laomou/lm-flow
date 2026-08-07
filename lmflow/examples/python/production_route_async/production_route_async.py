#!/usr/bin/env python3
"""Production-style metadata routing with asyncio output events."""

from __future__ import annotations

import asyncio
import pathlib
import shutil
import subprocess

import lmflow


@lmflow.kernel("ProductionDetector")
class ProductionDetector(lmflow.Kernel):
    def process(self, cc):
        source = cc.input(0)
        value = source.as_int()
        confidence = 0.92 if value % 3 == 0 else 0.62 if value % 3 == 1 else 0.25
        category = "person" if value % 2 == 0 else "vehicle"
        packet = lmflow.Packet.from_int(value)
        packet.set_metadata("confidence", confidence)
        packet.set_metadata("category", category)
        cc.emit(0, packet)


@lmflow.kernel("ProductionSink")
class ProductionSink(lmflow.Kernel):
    def process(self, cc):
        cc.forward(0, 0)


CONFIG = """
stats: full
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - name: detector
    kernel: ProductionDetector
    executor: cpu
    input_ports: [frames]
    output_ports: [detections]
    input_queues: { packets: 8 }
  - name: router
    type: route
    input_ports: [detections]
    output_ports: [high, review, rejected]
    mode: first
    unmatched: rejected
    routes:
      - to: high
        when:
          all:
            - { metadata: confidence, op: gte, value: 0.8 }
            - { metadata: category, op: eq, value: person }
      - to: review
        when: { metadata: confidence, op: gte, value: 0.5 }
      - { to: rejected, default: true }
  - { name: high_sink, kernel: ProductionSink, input_ports: [high], output_ports: [tracked] }
  - { name: review_sink, kernel: ProductionSink, input_ports: [review], output_ports: [reviewed] }
  - { name: rejected_sink, kernel: ProductionSink, input_ports: [rejected], output_ports: [] }
input_ports: [frames]
output_ports: [tracked, reviewed]
"""


async def consume(graph: lmflow.Graph, port: str, output: list[int]) -> None:
    async for event in graph.events(port):
        if isinstance(event, lmflow.PacketEvent):
            output.append(event.packet.as_int())
        elif isinstance(event, lmflow.DoneEvent):
            return


def write_diagnostics(graph: lmflow.Graph, directory: pathlib.Path, index: int) -> None:
    directory.mkdir(exist_ok=True)
    dot = directory / f"snapshot-{index:03d}.dot"
    dot.write_text(graph.to_dot(lmflow.DotView.DIAGNOSTICS), encoding="utf-8")
    graphviz = shutil.which("dot")
    if graphviz:
        subprocess.run(
            [graphviz, "-Tsvg", str(dot), "-o", str(dot.with_suffix(".svg"))],
            check=True,
        )


async def main() -> None:
    tracked: list[int] = []
    reviewed: list[int] = []
    with lmflow.Graph.from_yaml(CONFIG) as graph:
        tracked_task = asyncio.create_task(consume(graph, "tracked", tracked))
        reviewed_task = asyncio.create_task(consume(graph, "reviewed", reviewed))
        source = graph.input("frames")
        for value in range(12):
            source.send(value, ts=value)
            if value % 4 == 0:
                write_diagnostics(graph, pathlib.Path("lmflow-production-diagnostics"), value // 4)
        source.close()
        await asyncio.gather(tracked_task, reviewed_task)
        graph.wait_done(timeout=10.0)
        write_diagnostics(graph, pathlib.Path("lmflow-production-diagnostics"), 99)

    print(f"tracked={tracked}")
    print(f"reviewed={reviewed}")


if __name__ == "__main__":
    asyncio.run(main())
