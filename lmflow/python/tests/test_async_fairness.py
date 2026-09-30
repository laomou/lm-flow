import asyncio
import time
import unittest

import lmflow


@lmflow.kernel("FairnessSlowSink")
class SlowSink(lmflow.Kernel):
    def process(self, cc):
        cc.counter_add("processed")
        time.sleep(0.001)


def queued_graph():
    graph = lmflow.Graph.from_yaml("""
executors: [{name: host, type: DelegatingExecutor}]
nodes: [{kernel: FairnessSlowSink, executor: host, input_ports: [in]}]
input_ports: [in]
""")
    graph.start()
    graph.pause()
    inlet = graph.input("in")
    for value in range(300):
        inlet.send(value, ts=value)
    graph.close_all_inputs()
    graph.resume()
    return graph


class TestAsyncFairness(unittest.TestCase):
    def test_timer_runs_before_delegated_backlog_is_drained(self):
        async def scenario():
            with queued_graph() as graph:
                seen = []
                timer = asyncio.get_running_loop().call_later(
                    0.01, lambda: seen.append(graph.counter_value("processed")))
                try:
                    await graph.run_async(timeout=5)
                    await asyncio.sleep(0)
                    self.assertTrue(seen)
                    self.assertLess(seen[0], 300)
                    self.assertEqual(graph.counter_value("processed"), 300)
                finally:
                    timer.cancel()
        asyncio.run(scenario())

    def test_timeout_interrupts_delegated_backlog(self):
        async def scenario():
            with queued_graph() as graph:
                with self.assertRaises(lmflow.Timeout):
                    await graph.run_async(timeout=0.01, cancel_grace=1)
                self.assertLess(graph.counter_value("processed"), 300)
                self.assertEqual(graph.state, lmflow.GraphState.TERMINATED)
        asyncio.run(scenario())

    def test_task_cancellation_interrupts_delegated_backlog(self):
        async def scenario():
            with queued_graph() as graph:
                task = asyncio.create_task(graph.run_async(cancel_grace=1))
                timer = asyncio.get_running_loop().call_later(0.01, task.cancel)
                try:
                    with self.assertRaises(asyncio.CancelledError):
                        await task
                    self.assertLess(graph.counter_value("processed"), 300)
                    self.assertEqual(graph.state, lmflow.GraphState.TERMINATED)
                finally:
                    timer.cancel()
        asyncio.run(scenario())
