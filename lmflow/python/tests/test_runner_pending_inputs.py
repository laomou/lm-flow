import unittest
import weakref

import numpy as np
import lmflow


@lmflow.kernel("PendingInputCloseProbe")
class PendingInputCloseProbe(lmflow.Kernel):
    def process(self, cc):
        cc.forward(0, 0)


class TestRunnerPendingInputs(unittest.TestCase):
    def test_context_exit_discards_unprocessed_input_before_reuse(self):
        for started in (False, True):
            with self.subTest(started=started):
                runner = lmflow.KernelRunner("PendingInputCloseProbe", input_ports=1, output_ports=1)
                source = np.zeros(16, dtype=np.float32)
                reference = weakref.ref(source)
                with runner:
                    if started:
                        runner.start()
                    runner.add_input(0, source, ts=0)
                    self.assertFalse(source.flags.writeable)
                self.assertTrue(source.flags.writeable)
                del source
                self.assertIsNone(reference())
                with runner:
                    runner.add_input(0, 42, ts=1)
                    runner.process(1)
                    self.assertEqual(runner.try_next().as_int(), 42)
                    self.assertIsNone(runner.try_next())
