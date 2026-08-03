// passthrough.cc —— 零拷贝直通:把输入原样转发到输出(复用同一 payload,不拷贝)。
#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class PassThroughKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    cc.Forward(0, 0);  // 复用同一 payload,不拷贝
    return lmflow::Status::Ok();
  }
};
}  // namespace

void RegisterPassThroughKernel() {
  lmflow_register_kernel("PassThroughKernel", lmflow::KernelAdapter<PassThroughKernel>::vtable(),
                         nullptr);
}
