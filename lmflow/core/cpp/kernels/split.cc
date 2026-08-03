// split.cc —— 扇出:1 进 N 出,把同一输入 Forward 到每个输出口(共享同一 payload)。
#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class SplitKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetAny(0);
    for (size_t i = 0; i < c.NumOutputs(); ++i) c.OutputSetAny(i);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    for (size_t i = 0; i < cc.NumOutputs(); ++i) cc.Forward(0, i);  // 共享同一 payload
    return lmflow::Status::Ok();
  }
};
}  // namespace

void RegisterSplitKernel() {
  lmflow_register_kernel("SplitKernel", lmflow::KernelAdapter<SplitKernel>::vtable(), nullptr);
}
