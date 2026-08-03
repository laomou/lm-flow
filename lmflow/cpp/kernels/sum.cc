// sum.cc —— 有状态累加:跨包保持 total,Close 时在流尾单包位置吐出总和。
#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class SumKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }
  lmflow::Status Open(lmflow::Context&) override {
    total_ = 0;
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t v = 0;
    if (cc.Input(0).AsI64(&v)) total_ += v;
    return lmflow::Status::Ok();  // 中途不产出
  }
  lmflow::Status Close(lmflow::Context& cc) override {
    // 流尾单包位置:表示「整条流结束时的一个汇总结果」
    cc.Emit(0, lmflow::Packet::FromI64(total_).At(LMFLOW_TS_POST_STREAM));
    return lmflow::Status::Ok();
  }

 private:
  int64_t total_ = 0;
};
}  // namespace

void RegisterSumKernel() {
  lmflow_register_kernel("SumKernel", lmflow::KernelAdapter<SumKernel>::vtable(), nullptr);
}
