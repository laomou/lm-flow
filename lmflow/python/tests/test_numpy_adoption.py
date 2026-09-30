import unittest

import numpy as np
import lmflow


class TestNumpyAdoption(unittest.TestCase):
    def test_repeated_adoption_restores_writability_after_last_release(self):
        import gc

        for first_to_release in (0, 1):
            with self.subTest(first_to_release=first_to_release):
                source = np.arange(8, dtype=np.float32)
                packets = [lmflow.Packet.from_numpy(source), lmflow.Packet.from_numpy(source)]
                self.assertFalse(source.flags.writeable)
                packets[first_to_release] = None
                gc.collect()
                self.assertFalse(source.flags.writeable)
                with self.assertRaises(ValueError):
                    source[0] = -1
                packets[1 - first_to_release] = None
                gc.collect()
                self.assertTrue(source.flags.writeable)
                # The final release must remove the previous adoption state.
                packet = lmflow.Packet.from_numpy(source)
                self.assertFalse(source.flags.writeable)
                del packet
                self.assertTrue(source.flags.writeable)

    def test_repeated_adoption_preserves_original_readonly_flag(self):
        source = np.arange(8, dtype=np.float32)
        source.setflags(write=False)
        first = lmflow.Packet.from_numpy(source)
        second = lmflow.Packet.from_numpy(source)
        del first, second
        self.assertFalse(source.flags.writeable)
