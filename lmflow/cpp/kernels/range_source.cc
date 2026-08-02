// range_source.cc —— 源算子(0 输入)示范:每次产一个整数,发完 count 个后自报 SourceDone。
// 生成型算子的样板 —— 内核自产数据(相机 / 文件 / 合成 的合成版本)。有限源,产完自然终止全图。
// 注意:源节点必须挂线程池 executor(见 config 校验);无限源须自定速(阻塞)或由 host cancel。
#include <cstdint>

#include "flow.hpp"

#include "builtins.hpp"

namespace {
class RangeSourceKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);  // 只有输出、无输入 = 源节点
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    count_ = cc.OptionI64("count", 10);
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    if (next_ >= count_) {
      cc.SourceDone();  // 产完:引擎停止再触发本节点、关其输出边 → 图正常终止
      return lmflow::Status::Ok();
    }
    cc.Emit(0, lmflow::Packet::FromI64(next_));  // ts 省略 → 引擎赋单调时间戳
    ++next_;
    return lmflow::Status::Ok();
  }

 private:
  int64_t count_ = 10;
  int64_t next_ = 0;
};
}  // namespace

void RegisterRangeSourceKernel() {
  lmflow_register_kernel("RangeSourceKernel", lmflow::KernelAdapter<RangeSourceKernel>::vtable(),
                         nullptr);
}
