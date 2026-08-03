// affine.cc —— 逐元素仿射:输出 = 输入 * scale + shift(归一化的核心)。
// options: scale(默认 1)、shift(默认 0)、可选 dtype(输出 dtype,默认同输入)。
#include "flow.hpp"

#include "buffer_util.hpp"
#include "builtins.hpp"

namespace {
class AffineKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    scale_ = cc.OptionF64("scale", 1.0);
    shift_ = cc.OptionF64("shift", 0.0);
    const char* dt = cc.OptionStr("dtype", "");
    if (dt[0] != '\0') {
      out_dt_ = lmflow_bufutil::dtype_from_name(dt);
      if (out_dt_ < 0) return cc.Fail("options.dtype unknown/unsupported");
    }
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    LMFlowBuffer in{};
    if (!cc.Input(0).AsBuffer(&in)) return cc.Fail("input is not a buffer");
    if (!lmflow_bufutil::is_math_dtype(in.dtype)) return cc.Fail("input dtype unsupported (F16?)");
    if (!lmflow_bufutil::is_contiguous(in)) return cc.Fail("input buffer must be contiguous");

    const int32_t out_dt = out_dt_ >= 0 ? out_dt_ : in.dtype;  // 默认同输入 dtype
    LMFlowBuffer out{};
    lmflow::Packet p = lmflow::Packet::NewBuffer(in.ndim, in.shape, out_dt, &out);
    const int64_t n = lmflow_bufutil::elem_count(in);
    const size_t is = lmflow_dtype_size(in.dtype), os = lmflow_dtype_size(out_dt);
    const auto* src = static_cast<const uint8_t*>(in.data);
    auto* dst = static_cast<uint8_t*>(out.data);
    for (int64_t i = 0; i < n; ++i) {
      const double v = lmflow_bufutil::read_f64(src + i * is, in.dtype);
      lmflow_bufutil::write_f64(dst + i * os, out_dt, v * scale_ + shift_);
    }
    cc.Emit(0, std::move(p));
    return lmflow::Status::Ok();
  }

 private:
  double scale_ = 1.0, shift_ = 0.0;
  int32_t out_dt_ = -1;  // -1 = 同输入
};
}  // namespace

void RegisterAffineKernel() {
  lmflow_register_kernel("AffineKernel", lmflow::KernelAdapter<AffineKernel>::vtable(), nullptr);
}
