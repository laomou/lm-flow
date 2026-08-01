// mux.cc —— 多路选择:输入 0 = 控制口(I64 选择器),输入 1.. = 数据口 0,1,2…。
// 控制值 k 就把第 k 个数据口(输入 1+k)零拷贝转发到输出。
//
// 配**默认 sync 策略**用:所有输入口按时间戳对齐 → 控制与被选数据口同一时刻;
// 未被选中的数据口本时刻的包被 sync 消费后随上下文清理丢弃(不转发),故**不会积压**。
//
// (为何是 kernel 而非输入策略:push 模型里「只要求选中那一路」会让未选中口越积越多、
//  甚至因陈旧时间戳卡住;sync 全对齐 + kernel 读控制转发,既不积压也不卡,且引擎不碰 payload。)
//
// YAML:
//   - name: "mux"
//     kernel: "MuxKernel"
//     input_ports: ["select", "a", "b"]   # select=控制口;a=数据口0, b=数据口1
//     output_ports: ["out"]
#include "flow.hpp"

#include "builtins.hpp"

namespace {
class MuxKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);  // 输入 0 = 控制口
    c.OutputSetAny(0);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t k = 0;
    if (!cc.Input(0).AsI64(&k)) return cc.Fail("mux 控制口(输入 0)必须是 I64 选择器");
    const int64_t ndata = static_cast<int64_t>(cc.NumInputs()) - 1;  // 除控制口外
    if (k < 0 || k >= ndata) return cc.Fail("mux 选择器越界(数据口不够)");
    cc.Forward(static_cast<size_t>(1 + k), 0);  // 转发第 k 个数据口(零拷贝)
    return lmflow::Status::Ok();
  }
};
}  // namespace

void RegisterMuxKernel() {
  lmflow_register_kernel("MuxKernel", lmflow::KernelAdapter<MuxKernel>::vtable(), nullptr);
}
