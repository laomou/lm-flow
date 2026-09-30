import asyncio
import unittest

import lmflow


@lmflow.kernel("DoneEventLaterFailure")
class LaterFailure(lmflow.Kernel):
    def process(self, cc):
        raise RuntimeError("later branch failed")


def two_branches(right_kernel="PassThroughKernel"):
    return lmflow.Graph.from_yaml("""
nodes:
  - {kernel: PassThroughKernel, input_ports: [left], output_ports: [left_out]}
  - {kernel: %s, input_ports: [right], output_ports: [right_out]}
input_ports: [left, right]
output_ports: [left_out, right_out]
""" % right_kernel)


async def collect(events):
    return [event async for event in events]


class TestOutputDoneEvent(unittest.TestCase):
    def test_closed_output_finishes_before_other_input_is_closed(self):
        async def scenario():
            with two_branches() as graph:
                left = graph.events("left_out")
                right = graph.events("right_out")
                graph.start()
                graph.input("left").send(42, ts=7)
                graph.close_input("left")
                received = await asyncio.wait_for(collect(left), timeout=2)
                self.assertEqual(len(received), 3)
                self.assertEqual(received[0].packet.as_int(), 42)
                self.assertEqual(received[1], lmflow.TimestampBoundEvent(8))
                self.assertEqual(received[2], lmflow.DoneEvent())
                self.assertNotEqual(graph.state, lmflow.GraphState.TERMINATED)
                graph.close_input("right")
                self.assertEqual(await asyncio.wait_for(collect(right), 2), [lmflow.DoneEvent()])
                await graph.run_async(timeout=2, cancel_grace=1)
        asyncio.run(scenario())

    def test_already_waiting_empty_output_wakes_when_input_closes(self):
        async def scenario():
            with two_branches() as graph:
                left = graph.events("left_out")
                pending = asyncio.create_task(collect(left))
                await asyncio.sleep(0.01)
                self.assertFalse(pending.done())
                graph.close_input("left")
                self.assertEqual(await asyncio.wait_for(pending, 2), [lmflow.DoneEvent()])
                self.assertNotEqual(graph.state, lmflow.GraphState.TERMINATED)
                graph.close_input("right")
                await graph.run_async(timeout=2, cancel_grace=1)
        asyncio.run(scenario())

    def test_run_async_still_reports_failure_after_another_output_finishes(self):
        async def scenario():
            with two_branches("DoneEventLaterFailure") as graph:
                left = graph.events("left_out")
                graph.start()
                graph.close_input("left")
                self.assertEqual(await asyncio.wait_for(collect(left), 2), [lmflow.DoneEvent()])
                graph.input("right").send(1, ts=0)
                graph.close_input("right")
                with self.assertRaisesRegex(lmflow.KernelError, "later branch failed"):
                    await graph.run_async(timeout=2, cancel_grace=1)
        asyncio.run(scenario())

    def test_known_error_is_not_hidden_by_a_queued_done_event(self):
        async def scenario():
            with two_branches("DoneEventLaterFailure") as graph:
                left = graph.events("left_out")
                graph.start()
                graph.close_input("left")
                graph.input("right").send(1, ts=0)
                graph.close_input("right")
                with self.assertRaises(lmflow.KernelError):
                    graph.wait_done(timeout=2)
                with self.assertRaisesRegex(lmflow.KernelError, "later branch failed"):
                    await asyncio.wait_for(collect(left), 2)
        asyncio.run(scenario())
