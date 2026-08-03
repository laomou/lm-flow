// clamp.cc —— 逐元素钳位:输出 = clamp(输入, min, max)。dtype 不变。
// options: min(默认 -inf)、max(默认 +inf)—— 只设一侧即单侧钳位。
#include <limits>

#include "flow.hpp"

#include "buffer_util.hpp"
#include "builtins.hpp"

namespace {
class ClampKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    min_ = cc.OptionF64("min", -std::numeric_limits<double>::infinity());
    max_ = cc.OptionF64("max", std::numeric_limits<double>::infinity());
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    LMFlowBuffer in{};
    if (!cc.Input(0).AsBuffer(&in)) return cc.Fail("input is not a buffer");
    if (!lmflow_bufutil::is_math_dtype(in.dtype)) return cc.Fail("input dtype unsupported (F16?)");
    if (!lmflow_bufutil::is_contiguous(in)) return cc.Fail("input buffer must be contiguous");

    LMFlowBuffer out{};
    lmflow::Packet p = lmflow::Packet::NewBuffer(in.ndim, in.shape, in.dtype, &out);
    const int64_t n = lmflow_bufutil::elem_count(in);
    const size_t es = lmflow_dtype_size(in.dtype);
    const auto* src = static_cast<const uint8_t*>(in.data);
    auto* dst = static_cast<uint8_t*>(out.data);
    for (int64_t i = 0; i < n; ++i) {
      double v = lmflow_bufutil::read_f64(src + i * es, in.dtype);
      if (v < min_) v = min_;
      if (v > max_) v = max_;
      lmflow_bufutil::write_f64(dst + i * es, in.dtype, v);
    }
    cc.Emit(0, std::move(p));
    return lmflow::Status::Ok();
  }

 private:
  double min_ = 0.0, max_ = 0.0;
};
}  // namespace

void RegisterClampKernel() {
  lmflow_register_kernel("ClampKernel", lmflow::KernelAdapter<ClampKernel>::vtable(), nullptr);
}
