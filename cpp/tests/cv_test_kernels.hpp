// cv_test_kernels.hpp —— CV 操作**测试专用**算子(链 OpenCV;引擎本身一行 CV 都不含,ADR #14)。
//
// 用 flow_cv.hpp 把 LMFlowBuffer 当 cv::Mat 处理,演示「一个真正的 OpenCV C++ 算子」。
// 注册名一律带 `Test` 后缀,表明是测试专用、不与内置算子/用户算子混淆。
//
// 只依赖 flow.hpp(C++ 糖层)+ flow_cv.hpp(可选转换头)+ OpenCV。header-only,便于
// 被 C++ 图测试直接 include;将来若要从 Python 以插件方式加载,同一头也可复用。
#ifndef LMFLOW_CPP_TESTS_CV_TEST_KERNELS_HPP_
#define LMFLOW_CPP_TESTS_CV_TEST_KERNELS_HPP_

#include "flow.hpp"
#include "flow_cv.hpp"

namespace lmflow_test {

// 把输入图像取反(255 - x):最小但**真实**的 OpenCV 操作(cv::bitwise_not)。
// 取反是可逆的,便于断言:对同一张图跑两次应回到原图。
class CvInvertTestKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet p = cc.TakeInput(0);  // 取走 → 独占,CoW 零拷贝
    cv::Mat m;
    if (LMFlowStatus st = lmflow::CvMutable(p, &m)) return st;  // LMFlowBuffer → 可写 cv::Mat
    cv::bitwise_not(m, m);                                      // OpenCV 就地取反
    cc.Emit(0, std::move(p));
    return lmflow::Status::Ok();
  }
};

// 注册所有 CV 测试算子。名字带 `Test` 后缀,避免与内置/用户算子撞名。
inline void RegisterCvTestKernels() {
  lmflow_register_kernel("CvInvertTest", lmflow::KernelAdapter<CvInvertTestKernel>::vtable(),
                         nullptr);
}

}  // namespace lmflow_test

#endif  // LMFLOW_CPP_TESTS_CV_TEST_KERNELS_HPP_
