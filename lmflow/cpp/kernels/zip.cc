// zip.cc —— 汇合:2 进 1 出,按 tag(A/B)定位端口,时间戳对齐后两口都有值才产出和。
#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class ZipKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.InputSetBuiltin(1, LMFLOW_TYPE_I64);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }
  lmflow::Status Open(lmflow::Context& cc) override {
    // 按 tag 定位,不依赖 YAML 书写顺序 —— 端口声明形如:
    //     input_ports: ["A:left_stream", "B:right_stream"]
    // 于是不管两者谁先写、边名叫什么,LHS 永远是 tag 为 A 的那个口。
    lhs_ = cc.InputId("A");
    rhs_ = cc.InputId("B");
    // 若 YAML 没写 tag(input_ports: ["x","y"]),退回按声明顺序取序号 0/1。
    if (lhs_ == LMFLOW_INVALID_ID) lhs_ = 0;
    if (rhs_ == LMFLOW_INVALID_ID) rhs_ = 1;
    return lmflow::Status::Ok();
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t a = 0, b = 0;
    // 时间戳对齐后某口仍可能无数据(该时刻它就是没有)—— 这时不产出
    if (!cc.Input(lhs_).AsI64(&a) || !cc.Input(rhs_).AsI64(&b)) {
      return lmflow::Status::Ok();
    }
    cc.Emit(0, lmflow::Packet::FromI64(a + b));
    return lmflow::Status::Ok();
  }

 private:
  size_t lhs_ = 0, rhs_ = 1;
};
}  // namespace

void RegisterZipKernel() {
  lmflow_register_kernel("ZipKernel", lmflow::KernelAdapter<ZipKernel>::vtable(), nullptr);
}
