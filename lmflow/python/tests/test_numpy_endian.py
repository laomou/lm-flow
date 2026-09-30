import sys
import unittest

import numpy as np
import lmflow


class TestNumpyEndian(unittest.TestCase):
    def test_float16_rejects_non_native_byte_order(self):
        source = np.array([1.0, -2.0], dtype=np.dtype("float16").newbyteorder("S"))
        with self.assertRaisesRegex(ValueError, "byte order"):
            lmflow.Packet.from_numpy(source)
        self.assertTrue(source.flags.writeable)

    def test_float16_native_byte_order_roundtrip(self):
        for byte_order in ("=", "<" if sys.byteorder == "little" else ">"):
            source = np.array([1.0, -2.0], dtype=byte_order + "f2")
            packet = lmflow.Packet.from_numpy(source)
            np.testing.assert_array_equal(packet.as_numpy(), source)
