// reduce.cc —— 全缓冲归约成一个标量:sum / mean / min / max。演示 buffer → 标量(输出 F64)。
#include <cstring>
#include <limits>

#include "flow.hpp"

#include "buffer_util.hpp"
#include "builtins.hpp"

namespace {
class ReduceKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_F64);
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    const char* op = cc.OptionStr("op", "mean");
    if (!std::strcmp(op, "sum")) {
      op_ = Op::Sum;
    } else if (!std::strcmp(op, "mean")) {
      op_ = Op::Mean;
    } else if (!std::strcmp(op, "min")) {
      op_ = Op::Min;
    } else if (!std::strcmp(op, "max")) {
      op_ = Op::Max;
    } else {
      return cc.Fail("options.op must be one of sum/mean/min/max");
    }
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    LMFlowBuffer in{};
    if (!cc.Input(0).AsBuffer(&in)) return cc.Fail("input is not a buffer");
    if (!lmflow_bufutil::is_math_dtype(in.dtype)) return cc.Fail("input dtype unsupported (F16?)");
    if (!lmflow_bufutil::is_contiguous(in)) return cc.Fail("input buffer must be contiguous");

    const int64_t n = lmflow_bufutil::elem_count(in);
    const size_t es = lmflow_dtype_size(in.dtype);
    const auto* src = static_cast<const uint8_t*>(in.data);
    double acc = 0.0;
    double mn = std::numeric_limits<double>::infinity();
    double mx = -std::numeric_limits<double>::infinity();
    for (int64_t i = 0; i < n; ++i) {
      const double v = lmflow_bufutil::read_f64(src + i * es, in.dtype);
      acc += v;
      if (v < mn) mn = v;
      if (v > mx) mx = v;
    }
    double r = 0.0;
    if (n > 0) {
      switch (op_) {
        case Op::Sum: r = acc; break;
        case Op::Mean: r = acc / static_cast<double>(n); break;
        case Op::Min: r = mn; break;
        case Op::Max: r = mx; break;
      }
    }
    cc.Emit(0, lmflow::Packet::FromF64(r));
    return lmflow::Status::Ok();
  }

 private:
  enum class Op { Sum, Mean, Min, Max } op_ = Op::Mean;
};
}  // namespace

void RegisterReduceKernel() {
  lmflow_register_kernel("ReduceKernel", lmflow::KernelAdapter<ReduceKernel>::vtable(), nullptr);
}
