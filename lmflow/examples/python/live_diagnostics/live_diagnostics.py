#!/usr/bin/env python3
"""运行中周期导出 LMFlow Diagnostics DOT / SVG。

运行:
    python -m pip install -v .
    python lmflow/examples/python/live_diagnostics/live_diagnostics.py

输出写入当前目录的 ``lmflow-diagnostics/``。系统装有 Graphviz ``dot`` 时会同时生成 SVG；
没有 Graphviz 也不影响 DOT 快照。
"""

from __future__ import annotations

import pathlib
import shutil
import subprocess
import threading
import time

import lmflow

lmflow.register_builtin_kernels()


@lmflow.kernel("DiagnosticSlowPass")
class DiagnosticSlowPass(lmflow.Kernel):
    def process(self, context):
        time.sleep(0.03)
        _ = context.input(0)


GRAPH = """
executors:
  - { name: workers, num_threads: 2 }
nodes:
  - name: decode
    kernel: PassThrough
    input_ports: [in]
    output_ports: [decoded]
    executor: workers
  - name: slow_inference
    kernel: DiagnosticSlowPass
    input_ports: [decoded]
    output_ports: []
    executor: workers
    input_queues: { packets: 2 }
input_ports: [in]
"""


def write_snapshot(graph: lmflow.Graph, output: pathlib.Path, index: int) -> None:
    dot_path = output / f"snapshot-{index:03d}.dot"
    svg_path = dot_path.with_suffix(".svg")
    dot_path.write_text(
        graph.to_dot(view=lmflow.DotView.DIAGNOSTICS),
        encoding="utf-8",
    )
    graphviz = shutil.which("dot")
    if graphviz:
        subprocess.run(
            [graphviz, "-Tsvg", str(dot_path), "-o", str(svg_path)],
            check=True,
        )
        print(f"snapshot {index}: {svg_path}")
    else:
        print(f"snapshot {index}: {dot_path} (install Graphviz for SVG)")


def main() -> None:
    output = pathlib.Path("lmflow-diagnostics")
    output.mkdir(exist_ok=True)

    with lmflow.Graph.from_yaml(GRAPH) as graph:
        graph.start()
        source = graph.input("in")

        def produce() -> None:
            for value in range(40):
                source.send(value, ts=value)
            source.close()

        producer = threading.Thread(target=produce, name="producer")
        producer.start()

        index = 0
        while producer.is_alive():
            write_snapshot(graph, output, index)
            index += 1
            time.sleep(0.1)

        producer.join()
        write_snapshot(graph, output, index)
        index += 1
        graph.wait_done(timeout=10.0)
        write_snapshot(graph, output, index)


if __name__ == "__main__":
    main()
