// filter.cc —— 条件过滤:>= threshold 才转发;丢弃时必须推进时间戳边界,否则下游会一直等。
#include "lmflow/flow.hpp"

namespace {
class FilterKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    threshold_ = cc.OptionI64("threshold", 0);
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t v = 0;
    LMFLOW_RET_CHECK_MSG(cc, cc.Input(0).AsI64(&v), "input is not an integer packet");
    if (v >= threshold_) {
      cc.Forward(0, 0);
    } else {
      // 丢弃该包。必须告知下游「此刻之前不会再有数据」,否则下游会一直等。
      cc.SetNextTimestampBound(0, cc.InputTimestamp() + 1);
    }
    return lmflow::Status::Ok();
  }

 private:
  int64_t threshold_ = 0;
};
}  // namespace

LMFLOW_REGISTER_KERNEL(FilterKernel)
