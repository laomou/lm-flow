import unittest

import lmflow


class TestPollerLiveInput(unittest.TestCase):
    def test_idle_timeout_does_not_end_live_stream(self):
        with lmflow.Graph.from_yaml("""
nodes:
  - {kernel: PassThroughKernel, input_ports: [in], output_ports: [out]}
input_ports: [in]
output_ports: [out]
""") as graph:
            output = graph.add_poller("out")
            graph.start()
            with self.assertRaises(TimeoutError):
                output.next(timeout=0.02)
            graph.input("in").send(42, ts=0)
            self.assertEqual(output.next(timeout=2).as_int(), 42)
            graph.close_all_inputs()
            self.assertIsNone(output.next(timeout=2))
