import unittest

import lmflow


@lmflow.kernel("InvalidEmission")
class InvalidEmission(lmflow.Kernel):
    def process(self, cc):
        cc.emit(0, 123)
        cc.emit(9, 456)


class TestEmissionErrors(unittest.TestCase):
    def test_graph_reports_error_and_discards_partial_output(self):
        with lmflow.Graph.from_yaml("""
nodes:
  - {kernel: InvalidEmission, input_ports: [in], output_ports: [out]}
input_ports: [in]
output_ports: [out]
""") as graph:
            output = graph.add_poller("out")
            graph.start()
            graph.input("in").send(0, ts=0)
            graph.close_all_inputs()
            with self.assertRaisesRegex(lmflow.KernelError, "out of range"):
                graph.wait_done(timeout=2)
            with self.assertRaises(lmflow.KernelError):
                output.try_next()

    def test_runner_reports_output_error(self):
        with lmflow.KernelRunner("InvalidEmission", input_ports=1, output_ports=1) as runner:
            runner.add_input(0, 0, ts=0)
            with self.assertRaisesRegex(lmflow.KernelError, "out of range"):
                runner.process(0)
            self.assertIsNone(runner.try_next())
