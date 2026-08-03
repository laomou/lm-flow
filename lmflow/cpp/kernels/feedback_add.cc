// feedback_add.cc —— 反馈相加:out = 正向输入 + 反馈(back-edge 回灌,空则按 0)。
// 演示 back-edge(最新值反馈寄存器):端口 1 由图标为 back_edges,首拍(尚无反馈)为空按 0。
// 接成自环(out 回灌到本节点)时即「运行累加」:out(t) = in(t) + out(t-1)。
#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class FeedbackAddKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);  // 正向输入(驱动本节点触发)
    c.InputSetBuiltin(1, LMFLOW_TYPE_I64);  // 反馈输入(back-edge,可空)
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t v = 0;
    if (!cc.Input(0).AsI64(&v)) return lmflow::Status::Ok();  // 无正向输入 → 不产出
    int64_t fb = 0;
    cc.Input(1).AsI64(&fb);  // 反馈可空(首拍 / 暂无反馈)→ 按 0
    cc.Emit(0, lmflow::Packet::FromI64(v + fb));
    return lmflow::Status::Ok();
  }
};
}  // namespace

void RegisterFeedbackAddKernel() {
  lmflow_register_kernel("FeedbackAddKernel", lmflow::KernelAdapter<FeedbackAddKernel>::vtable(),
                         nullptr);
}
