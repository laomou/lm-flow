import unittest

import lmflow


@lmflow.kernel("InitializationOutput")
class InitializationOutput(lmflow.Kernel):
    def open(self, cc):
        cc.emit(0, 42, ts=-1)

    def process(self, cc):
        cc.forward(0, 0)


class TestOpenOutputs(unittest.TestCase):
    def test_graph_publishes_initialization_before_process(self):
        with lmflow.Graph.from_yaml("""
nodes:
  - {kernel: InitializationOutput, input_ports: [in], output_ports: [out]}
input_ports: [in]
output_ports: [out]
""") as graph:
            output = graph.add_poller("out")
            graph.start()
            self.assertEqual(output.try_next().as_int(), 42)
            graph.input("in").send(7, ts=0)
            graph.close_all_inputs()
            graph.wait_done(timeout=2)
            self.assertEqual(output.try_next().as_int(), 7)
            self.assertIsNone(output.try_next())

    def test_runner_preserves_implicit_open_output(self):
        with lmflow.KernelRunner("InitializationOutput", input_ports=1, output_ports=1) as runner:
            runner.add_input(0, 7, ts=0)
            runner.process(0)
            self.assertEqual(runner.try_next().as_int(), 42)
            self.assertEqual(runner.try_next().as_int(), 7)
            self.assertIsNone(runner.try_next())
