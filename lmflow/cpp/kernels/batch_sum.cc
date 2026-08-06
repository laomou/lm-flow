// batch_sum.cc —— 批处理样板:每批求和,一批产一个和。
// 配 input_policy: { type: batch, capacity: N }:引擎攒够 N 个包一次交给本算子;
// 用 InputCount + InputAt 遍历整批(关流时余批可能不足 N)。演示 batch 输入策略。
// 输出时间戳继承 input_ts(= 批内最后一个包的时间戳),下游单调。
#include "lmflow/flow.hpp"

namespace {
class BatchSumKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t sum = 0;
    size_t n = cc.InputCount(0);
    for (size_t k = 0; k < n; ++k) {
      int64_t v = 0;
      if (cc.InputAt(0, k).AsI64(&v)) sum += v;
    }
    cc.Emit(0, lmflow::Packet::FromI64(sum));
    return lmflow::Status::Ok();
  }
};
}  // namespace

LMFLOW_REGISTER_KERNEL(BatchSumKernel)
