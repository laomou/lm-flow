import unittest

import lmflow


@lmflow.kernel("RepeatedOutputRunner")
class RepeatedOutputRunner(lmflow.Kernel):
    def process(self, cc):
        cc.forward(0, 0)

    def close(self, cc):
        cc.emit(0, 99, ts=99)


class TestRunnerRepeated(unittest.TestCase):
    def test_repeated_processing_and_close_output(self):
        with lmflow.KernelRunner("RepeatedOutputRunner", input_ports=1, output_ports=1) as runner:
            for value in range(3):
                runner.add_input(0, value, ts=value)
                runner.process(value)
                packet = runner.try_next()
                self.assertIsNotNone(packet)
                self.assertEqual(packet.as_int(), value)
            runner.close()
            self.assertEqual(runner.try_next().as_int(), 99)
            self.assertIsNone(runner.try_next())
