// affine.cc —— 逐元素仿射:输出 = 输入 * scale + shift(归一化的核心)。
// options: scale(默认 1)、shift(默认 0)、可选 dtype(输出 dtype,默认同输入)。
#include "lmflow/flow.hpp"

#include "buffer_util.hpp"
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
      LMFLOW_RET_CHECK_MSG(cc, out_dt_ >= 0, "options.dtype unknown/unsupported");
    }
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    LMFlowBuffer in{};
    LMFLOW_RET_CHECK_MSG(cc, cc.Input(0).AsBuffer(&in), "input is not a buffer");
    LMFLOW_RET_CHECK_MSG(cc, lmflow_bufutil::is_math_dtype(in.dtype),
                         "input dtype is not a supported numeric dtype");
    LMFLOW_RET_CHECK_MSG(cc, lmflow_bufutil::is_contiguous(in), "input buffer must be contiguous");

    const int32_t out_dt = out_dt_ >= 0 ? out_dt_ : in.dtype;  // 默认同输入 dtype
    LMFlowBuffer out{};
    lmflow::Packet p = lmflow::Packet::NewBuffer(in.ndim, in.shape, out_dt, &out);
    const int64_t n = lmflow_bufutil::elem_count(in);
    const size_t is = lmflow_dtype_size(in.dtype), os = lmflow_dtype_size(out_dt);
    const auto* src = static_cast<const uint8_t*>(in.data);
    auto* dst = static_cast<uint8_t*>(out.data);
    // 快路:同 dtype 且为浮点。下面的通用循环每元素做两次 dtype 分派(read_f64 / write_f64)
    // 并绕一趟 double,编译器无法向量化 —— 实测约 20 ns/元素(Adreno 740,1080p→540x960 那档
    // 31 ms 的差值几乎全出自这里;归因见 adapters/vulkan/benchmarks/chain_bench.cc)。把分派
    // 提到循环外,就成了一条可向量化的乘加。
    //
    // 运算仍走 double,故结果与通用路径**位等价** —— 这条快路不改变任何输出。
    //
    // 只覆盖 f32/f64:整型目标必须保留 write_f64 的范围 clamp + 就近取整语义,复制那段逻辑
    // 风险大于收益;f16 还要过 half 转换。热点本来也在 f32 前处理(cast→affine→clamp)。
    // clamp.cc / cast.cc / reduce.cc 是同一个模式,可按同样形状特化 —— 但应各自实测后再动。
    if (out_dt == in.dtype && in.dtype == LMFLOW_DTYPE_F32) {
      const auto* s = static_cast<const float*>(in.data);
      auto* d = static_cast<float*>(out.data);
      for (int64_t i = 0; i < n; ++i) {
        d[i] = static_cast<float>(static_cast<double>(s[i]) * scale_ + shift_);
      }
    } else if (out_dt == in.dtype && in.dtype == LMFLOW_DTYPE_F64) {
      const auto* s = static_cast<const double*>(in.data);
      auto* d = static_cast<double*>(out.data);
      for (int64_t i = 0; i < n; ++i) d[i] = s[i] * scale_ + shift_;
    } else {
      for (int64_t i = 0; i < n; ++i) {
        const double v = lmflow_bufutil::read_f64(src + i * is, in.dtype);
        lmflow_bufutil::write_f64(dst + i * os, out_dt, v * scale_ + shift_);
      }
    }
    cc.Emit(0, std::move(p));
    return lmflow::Status::Ok();
  }

 private:
  double scale_ = 1.0, shift_ = 0.0;
  int32_t out_dt_ = -1;  // -1 = 同输入
};
}  // namespace

LMFLOW_REGISTER_KERNEL(AffineKernel)
