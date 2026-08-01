// invert.cc —— 原地改写:CoW 省拷贝路径示范。TakeInput 取走输入后 MakeMutableBuffer,
// 独占时零拷贝就地取反;被上游 Split 共享时才复制,保证不污染其它分支。
#include <cstdint>

#include "flow.hpp"

#include "builtins.hpp"

namespace {
class InvertKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    // 关键第一步:把包从输入槽**取走**。否则上下文仍持一份引用,CoW 必然复制。
    lmflow::Packet p = cc.TakeInput(0);
    LMFlowBuffer buf{};
    if (LMFlowStatus st = p.MakeMutableBuffer(&buf)) return st;
    if (buf.dtype != LMFLOW_DTYPE_U8 || buf.ndim < 2) return LMFLOW_ERR_INVALID_ARG;

    const size_t row_bytes = static_cast<size_t>(buf.shape[1]) *
                             (buf.ndim >= 3 ? static_cast<size_t>(buf.shape[2]) : 1);
    for (int64_t y = 0; y < buf.shape[0]; ++y) {
      auto* line = static_cast<uint8_t*>(buf.data) + y * buf.strides[0];
      for (size_t x = 0; x < row_bytes; ++x) line[x] = static_cast<uint8_t>(255 - line[x]);
    }
    cc.Emit(0, std::move(p));
    return lmflow::Status::Ok();
  }
};
}  // namespace

void RegisterInvertKernel() {
  lmflow_register_kernel("InvertKernel", lmflow::KernelAdapter<InvertKernel>::vtable(), nullptr);
}
