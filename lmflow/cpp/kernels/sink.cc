// sink.cc —— 汇点:只消费不产出(零输出口),走引擎日志与按图计数器,不抢宿主 stdout。
#include <cstdio>

#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class SinkKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) { c.InputSetAny(0); }
  lmflow::Status Process(lmflow::Context& cc) override {
    // 走引擎日志而非 printf:库不该抢占宿主的 stdout
    char buf[64];
    snprintf(buf, sizeof(buf), "received packet @ ts=%lld",
             static_cast<long long>(cc.InputTimestamp()));
    cc.Log(LMFLOW_LOG_DEBUG, buf);
    cc.CounterAdd("sink.packets");
    ++count_;
    return lmflow::Status::Ok();
  }
  lmflow::Status Close(lmflow::Context& cc) override {
    char buf[64];
    snprintf(buf, sizeof(buf), "processed %lld packets in total", static_cast<long long>(count_));
    cc.LogInfo(buf);
    // 计数器是**按图**的,比全局日志更适合被测试断言
    cc.CounterAdd("sink.closed");
    return lmflow::Status::Ok();
  }

 private:
  long long count_ = 0;
};
}  // namespace

void RegisterSinkKernel() {
  lmflow_register_kernel("SinkKernel", lmflow::KernelAdapter<SinkKernel>::vtable(), nullptr);
}
