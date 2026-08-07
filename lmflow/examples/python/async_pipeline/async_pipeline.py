#!/usr/bin/env python3
"""Asyncio host example with deterministic graph shutdown.

The graph mixes a Python kernel on a DelegatingExecutor with a bundled C++
kernel on a worker pool. ``Graph.run_async`` drives delegated work from the
event-loop thread without polling.

Run after installing the wheel or repository checkout:

    python lmflow/examples/python/async_pipeline/async_pipeline.py
"""

from __future__ import annotations

import argparse
import asyncio

import lmflow


@lmflow.kernel("AsyncOffset")
class AsyncOffset(lmflow.Kernel):
    def open(self, context):
        self.offset = context.option_int("offset", 0)

    def process(self, context):
        context.emit(0, context.input(0).as_int() + self.offset)


GRAPH = """
stats: full
executors:
  - { name: host, type: DelegatingExecutor }
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - name: scale
    kernel: ScaleKernel
    executor: cpu
    input_ports: [in]
    output_ports: [scaled]
    options: { factor: 10 }
  - name: offset
    kernel: AsyncOffset
    executor: host
    input_ports: [scaled]
    output_ports: [out]
    options: { offset: 1 }
input_ports: [in]
output_ports: [out]
"""


async def consume(events: lmflow.AsyncOutputEvents) -> list[int]:
    values = []
    async for event in events:
        if isinstance(event, lmflow.PacketEvent):
            values.append(event.packet.as_int())
    return values


async def run_pipeline(count: int, timeout: float) -> list[int]:
    with lmflow.Graph.from_yaml(GRAPH) as graph:
        events = graph.events("out")
        runner = asyncio.create_task(
            graph.run_async(timeout=timeout, cancel_grace=1.0)
        )
        consumer = asyncio.create_task(consume(events))
        await asyncio.sleep(0)

        async def produce() -> None:
            source = graph.input("in")
            try:
                for value in range(count):
                    source.send(value, ts=value)
                    await asyncio.sleep(0)
                source.close()
            except RuntimeError:
                # A timeout/cancellation closes the graph while a producer may
                # still be between two sends. The runner owns final cleanup.
                if not runner.done():
                    raise

        producer = asyncio.create_task(produce())

        try:
            await runner
            await producer
            return await consumer
        except asyncio.CancelledError:
            producer.cancel()
            consumer.cancel()
            await asyncio.gather(producer, consumer, return_exceptions=True)
            raise
        except lmflow.Timeout:
            producer.cancel()
            consumer.cancel()
            await asyncio.gather(producer, consumer, return_exceptions=True)
            raise


async def main_async(count: int, timeout: float) -> None:
    try:
        values = await run_pipeline(count, timeout)
    except lmflow.Timeout as error:
        raise SystemExit(f"pipeline timed out: {error}") from error
    except asyncio.CancelledError:
        print("pipeline cancelled; graph shutdown completed")
        raise

    expected = [value * 10 + 1 for value in range(count)]
    if values != expected:
        raise RuntimeError(f"unexpected output: {values}")
    print(f"processed {len(values)} packets: {values}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--count", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=5.0)
    args = parser.parse_args()
    if args.count < 0:
        parser.error("--count must be non-negative")
    if args.timeout < 0:
        parser.error("--timeout must be non-negative")
    try:
        asyncio.run(main_async(args.count, args.timeout))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
