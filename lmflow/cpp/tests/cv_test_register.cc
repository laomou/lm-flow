// cv_test_register.cc —— 把 CV 测试算子的注册以 extern "C" 暴露,供 Python 扩展在
// **带 --with-cv-test 构建**时调用(见 python/build.py、python/src/bindings.cc)。
//
// 为何单独一个编译单元:它 include flow_cv.hpp → flow.hpp(namespace lmflow),而
// bindings.cc 自己也有 namespace lmflow;放同一 TU 会撞名。分开编译即互不影响,
// 链接时 bindings.cc 通过这个 extern "C" 符号调到它。
#include "cv_test_kernels.hpp"

extern "C" void lmflow_register_cv_test_kernels(void) { lmflow_test::RegisterCvTestKernels(); }
