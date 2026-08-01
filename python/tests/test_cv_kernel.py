"""Python 送 numpy 图像 → C++ OpenCV 算子 → 读回:直接证明二者是同一块内存/同一 pattern。

前提:扩展需以 `python python/build.py --with-cv-test` 构建(把 CvInvertTest 编进扩展、
链 OpenCV)。生产扩展不含 OpenCV(ADR #14),此时整个用例自动跳过。

这条用例回答了「怎么证明 numpy 与 cv::Mat 底层是同一个 LMFlowBuffer」:不是靠两个分开
测试的传递推理,而是**一条数据路** —— 同一张已知 numpy 图进、经真实 cv::bitwise_not 出,
逐字节等于 255-原图,即证明 C++ 那侧的 cv::Mat 看到的就是 Python 送的那块 numpy 内存。
"""

import unittest

import numpy as np

import lmflow


@unittest.skipUnless(
    lmflow.has_cv_test_kernels(),
    "扩展未带 --with-cv-test 构建(无 CvInvertTest;生产扩展零 OpenCV)",
)
class TestPythonNumpyThroughCppCvKernel(unittest.TestCase):
    YAML = """
nodes:
  - { name: inv, kernel: CvInvertTest, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""

    def test_invert_roundtrip(self):
        lmflow.register_cv_test_kernels()  # 注册 C++ 侧 CvInvertTest
        with lmflow.Graph.from_yaml(self.YAML) as g:
            out = g.add_poller("out")
            g.start()
            # 一张已知 2x3 U8 图(cv2.imread 返回的也是这种 numpy ndarray)
            img = np.array([[0, 10, 20], [30, 40, 50]], dtype=np.uint8)
            g.input("in").send(img, ts=0)

            res = out.next(timeout=5.0).as_numpy()
            # C++ 侧 cv::bitwise_not 逐像素 255-x。逐字节相等 ⟹ Python 送的 numpy
            # 与 C++ 读到的 cv::Mat 是同一块内存、同一 pattern(直接证明)。
            self.assertEqual(res.shape, img.shape)
            self.assertEqual(res.dtype, np.uint8)
            self.assertTrue(np.array_equal(res, 255 - img), f"期望 {255 - img},得到 {res}")

            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_larger_image_channels(self):
        """多通道 + 更大尺寸也逐字节对上(排除 shape/stride/通道错位)。"""
        lmflow.register_cv_test_kernels()
        with lmflow.Graph.from_yaml(self.YAML) as g:
            out = g.add_poller("out")
            g.start()
            rng = np.arange(4 * 5 * 3, dtype=np.uint8).reshape(4, 5, 3)  # HWC 彩色图
            g.input("in").send(rng, ts=0)
            res = out.next(timeout=5.0).as_numpy()
            self.assertEqual(res.shape, (4, 5, 3))
            self.assertTrue(np.array_equal(res, 255 - rng))
            g.close_all_inputs()
            g.wait_done(timeout=5.0)


if __name__ == "__main__":
    unittest.main()
