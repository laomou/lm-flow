// scale.cc —— 参数化数值变换:读 options.factor,输出 输入×factor(示范读参数 + 类型声明)。
#include "flow.hpp"

#include "builtins.hpp"

namespace {
class ScaleKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    factor_ = cc.OptionI64("factor", 1);
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t v = 0;
    if (!cc.Input(0).AsI64(&v)) return cc.Fail("输入不是整数包");
    cc.Emit(0, lmflow::Packet::FromI64(v * factor_));
    return lmflow::Status::Ok();
  }

 private:
  int64_t factor_ = 1;
};
}  // namespace

void RegisterScaleKernel() {
  lmflow_register_kernel("ScaleKernel", lmflow::KernelAdapter<ScaleKernel>::vtable(), nullptr);
}
