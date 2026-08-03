// stringify.cc —— 类型转换:int64 输入 -> UTF-8 字符串输出(异类型输入输出示范)。
#include <string>

#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class StringifyKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_STR);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t v = 0;
    LMFLOW_RET_CHECK_MSG(cc, cc.Input(0).AsI64(&v), "input is not an integer packet");
    cc.Emit(0, lmflow::Packet::FromStr(std::to_string(v).c_str()));
    return lmflow::Status::Ok();
  }
};
}  // namespace

void RegisterStringifyKernel() {
  lmflow_register_kernel("StringifyKernel", lmflow::KernelAdapter<StringifyKernel>::vtable(),
                         nullptr);
}
