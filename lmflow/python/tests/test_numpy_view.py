import unittest

import numpy as np
import lmflow


class TestNumpyView(unittest.TestCase):
    def test_numpy_view_retains_payload_across_copy_on_write(self):
        import gc
        import weakref

        source = np.arange(8, dtype=np.float32)
        source_ref = weakref.ref(source)
        packet = lmflow.Packet.from_numpy(source)
        view = packet.as_numpy()
        del source
        writable = packet.make_mutable()
        gc.collect()
        self.assertIsNotNone(source_ref())
        writable[:] = -1
        np.testing.assert_array_equal(view, np.arange(8, dtype=np.float32))
        del view
        gc.collect()
        self.assertIsNone(source_ref())

    def test_writable_view_retains_payload_across_copy_on_write(self):
        packet = lmflow.Packet.from_numpy(np.arange(8, dtype=np.float32))
        first = packet.make_mutable()
        second = packet.make_mutable()
        self.assertFalse(np.shares_memory(first, second))
        second[:] = -1
        del packet, second
        np.testing.assert_array_equal(first, np.arange(8, dtype=np.float32))

    def test_observer_view_retains_borrowed_payload_after_callback(self):
        import gc
        import weakref

        source = np.arange(8, dtype=np.float32)
        source_ref = weakref.ref(source)
        saved = []
        with lmflow.Graph.from_yaml("""
nodes:
  - {kernel: PassThroughKernel, input_ports: [in], output_ports: [out]}
input_ports: [in]
output_ports: [out]
""") as graph:
            graph.observe("out", lambda packet: saved.append(packet.as_numpy()))
            graph.start()
            graph.input("in").send(source, ts=0)
            graph.close_all_inputs()
            graph.wait_done(timeout=5.0)
        del source
        gc.collect()
        self.assertIsNotNone(source_ref())
        np.testing.assert_array_equal(saved[0], np.arange(8, dtype=np.float32))
        saved.clear()
        gc.collect()
        self.assertIsNone(source_ref())
