import unittest
import weakref

import numpy as np
import lmflow


@lmflow.kernel("RunnerConsumeInput")
class RunnerConsumeInput(lmflow.Kernel):
    def process(self, cc):
        pass


@lmflow.kernel("RunnerFailInput")
class RunnerFailInput(lmflow.Kernel):
    def process(self, cc):
        raise RuntimeError("input processing failed")


class TestRunnerInputLifetime(unittest.TestCase):
    def test_consumed_numpy_input_is_released_on_success_and_failure(self):
        for kernel in ("RunnerConsumeInput", "RunnerFailInput"):
            with self.subTest(kernel=kernel):
                with lmflow.KernelRunner(kernel, input_ports=1, output_ports=1) as runner:
                    source = np.zeros(16, dtype=np.float32)
                    reference = weakref.ref(source)
                    runner.add_input(0, source, ts=0)
                    self.assertFalse(source.flags.writeable)
                    if kernel == "RunnerFailInput":
                        with self.assertRaises(lmflow.KernelError):
                            runner.process(0)
                    else:
                        runner.process(0)
                    self.assertIsNone(runner.try_next())
                    self.assertTrue(source.flags.writeable)
                    del source
                    self.assertIsNone(reference())
