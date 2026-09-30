import subprocess
import sys
import textwrap
import unittest

import lmflow


class TestPacketTransfer(unittest.TestCase):
    def test_owned_packet_is_emptied_and_cannot_be_submitted_twice(self):
        with lmflow.KernelRunner("PassThroughKernel", input_ports=1, output_ports=1) as runner:
            packet = lmflow.Packet.from_int(42)
            runner.add_input(0, packet, ts=7)
            self.assertTrue(packet.is_empty)
            self.assertIsNone(packet.as_int())
            with self.assertRaisesRegex(ValueError, "transferred"):
                runner.add_input(0, packet, ts=8)
            with self.assertRaisesRegex(ValueError, "transferred"):
                packet.set_metadata("key", "value")
            runner.process(7)
            output = runner.try_next()
            self.assertEqual(output.as_int(), 42)
            self.assertEqual(output.timestamp, 7)

    def test_rejected_send_still_consumes_owned_packet(self):
        with lmflow.Graph.from_yaml("""
nodes: [{kernel: PassThroughKernel, input_ports: [in], output_ports: [out]}]
input_ports: [in]
output_ports: [out]
""") as graph:
            graph.start()
            graph.close_all_inputs()
            packet = lmflow.Packet.from_int(42)
            with self.assertRaises(RuntimeError):
                graph.input("in").send(packet, ts=0)
            self.assertTrue(packet.is_empty)
            with self.assertRaisesRegex(ValueError, "transferred"):
                graph.input("in").send(packet, ts=1)

    def test_borrowed_fanout_retains_payload_with_per_output_timestamps(self):
        # Isolate the pre-fix dangling references. Exit without destructing them if
        # ownership has already been lost, so this regression fails without a UAF.
        code = r'''
import gc
import os
import weakref
import numpy as np
import lmflow

@lmflow.kernel("TransferBorrowedFanout")
class Fanout(lmflow.Kernel):
    def process(self, cc):
        packet = cc.input(0)
        cc.emit(0, packet, ts=9)
        cc.emit(1, packet, ts=10)

source = np.arange(8, dtype=np.float32)
source_ref = weakref.ref(source)
runner = lmflow.KernelRunner("TransferBorrowedFanout", input_ports=1, output_ports=2)
runner.add_input(0, source, ts=5)
runner.process(5)
first = runner.try_next(0)
second = runner.try_next(1)
del source
runner.close()
gc.collect()
if source_ref() is None:
    os._exit(3)
assert first.timestamp == 9 and second.timestamp == 10
np.testing.assert_array_equal(first.as_numpy(), np.arange(8, dtype=np.float32))
del first
gc.collect()
assert source_ref() is not None
np.testing.assert_array_equal(second.as_numpy(), np.arange(8, dtype=np.float32))
del second
gc.collect()
assert source_ref() is None
'''
        result = subprocess.run([sys.executable, "-c", textwrap.dedent(code)],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_borrowed_mutations_require_take_input(self):
        code = r'''
import os
import numpy as np
import lmflow

@lmflow.kernel("TransferBorrowedMutation")
class Borrowed(lmflow.Kernel):
    def process(self, cc):
        packet = cc.input(0)
        for mutate in (lambda: packet.set_metadata("key", 1),
                       lambda: packet.remove_metadata("key"), packet.make_mutable):
            try:
                mutate()
            except ValueError as error:
                assert "take_input" in str(error)
            else:
                os._exit(2)
        owned = cc.take_input(0)
        owned.set_metadata("key", 1)
        writable = owned.make_mutable()
        writable[:] = 4
        cc.emit(0, owned)

with lmflow.KernelRunner("TransferBorrowedMutation", input_ports=1, output_ports=1) as runner:
    runner.add_input(0, np.zeros(3, dtype=np.float32), ts=0)
    runner.process(0)
    output = runner.try_next()
    assert output.metadata("key") == 1
    np.testing.assert_array_equal(output.as_numpy(), [4, 4, 4])
'''
        result = subprocess.run([sys.executable, "-c", textwrap.dedent(code)],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
