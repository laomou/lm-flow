// cast.cc —— dtype 转换:输出 = 输入按 options.dtype 重新量化(如 u8 → f32)。
// BUFFER 数值算子:读输入(任意受支持 dtype)统一走 double,再写成目标 dtype(整型 clamp+round)。
#include "lmflow/flow.hpp"

#include "buffer_util.hpp"
#include "builtins.hpp"

namespace {
class CastKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    out_dt_ = lmflow_bufutil::dtype_from_name(cc.OptionStr("dtype", "f32"));
    LMFLOW_RET_CHECK_MSG(cc, out_dt_ >= 0,
                         "options.dtype unknown/unsupported (u8/i8/u16/i16/i32/i64/f16/f32/f64)");
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    LMFlowBuffer in{};
    // 用 LMFLOW_RET_CHECK_MSG:失败时自动带上表达式与 file:line,定位不靠猜。
    LMFLOW_RET_CHECK_MSG(cc, cc.Input(0).AsBuffer(&in), "input is not a buffer");
    LMFLOW_RET_CHECK_MSG(cc, lmflow_bufutil::is_math_dtype(in.dtype),
                         "input dtype is not a supported numeric dtype");
    LMFLOW_RET_CHECK_MSG(cc, lmflow_bufutil::is_contiguous(in), "input buffer must be contiguous");

    LMFlowBuffer out{};
    lmflow::Packet p = lmflow::Packet::NewBuffer(in.ndim, in.shape, out_dt_, &out);
    const int64_t n = lmflow_bufutil::elem_count(in);
    const size_t is = lmflow_dtype_size(in.dtype), os = lmflow_dtype_size(out_dt_);
    const auto* src = static_cast<const uint8_t*>(in.data);
    auto* dst = static_cast<uint8_t*>(out.data);
    for (int64_t i = 0; i < n; ++i) {
      lmflow_bufutil::write_f64(dst + i * os, out_dt_,
                                lmflow_bufutil::read_f64(src + i * is, in.dtype));
    }
    cc.Emit(0, std::move(p));
    return lmflow::Status::Ok();
  }

 private:
  int32_t out_dt_ = LMFLOW_DTYPE_F32;
};
}  // namespace

void RegisterCastKernel() {
  lmflow_register_kernel("CastKernel", lmflow::KernelAdapter<CastKernel>::vtable(), nullptr);
}
