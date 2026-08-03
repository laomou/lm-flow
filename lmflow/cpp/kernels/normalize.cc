// normalize.cc —— 参数用法示范:必需参数(RequireOption)、数组参数(OptionArray)、
// 嵌套点号路径(roi.x)、side packet(RequireSidePacket/HasSidePacket)。
//
// YAML:
//   - name: "norm"
//     kernel: "NormalizeKernel"
//     input_ports: ["in"]
//     output_ports: ["out"]
//     options:
//       scale: 0.00392156862   # 必需:缺了就在 Open 阶段直接失败
//       mean:  [0.485, 0.456, 0.406]
//       std:   [0.229, 0.224, 0.225]
//       roi:   { x: 8, y: 8 }  # 嵌套:用点号路径读
#include <cstdio>

#include "lmflow/flow.hpp"

#include "builtins.hpp"

namespace {
class NormalizeKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
    // 声明必需的 side packet:宿主漏注入 → init 阶段就报错,不必等到 open
    c.RequireSidePacket("calibration");
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    // 必需参数:拼错 key 或漏配会当场失败,并带上可读原因(不是静默用默认值跑歪)
    LMFLOW_RET_CHECK_MSG(cc, cc.RequireOption("scale", &scale_) == LMFLOW_OK,
                         "options.scale missing or type mismatch (required option)");

    // 数组参数:归一化均值/标准差这类配置的常见形态
    n_mean_ = cc.OptionArray("mean", mean_, 4);
    n_std_ = cc.OptionArray("std", std_, 4);

    // 嵌套参数:点号路径,无需自己解析 JSON
    roi_x_ = static_cast<int>(cc.OptionI64("roi.x", 0));
    roi_y_ = static_cast<int>(cc.OptionI64("roi.y", 0));

    // side packet:宿主注入的常量对象(此处示意读取一个可选的标定表)
    has_calib_ = cc.HasSidePacket("calibration");

    // 走引擎日志系统(引擎会自动加节点名前缀),不抢 stdout
    char buf[160];
    snprintf(buf, sizeof(buf), "scale=%g mean=%zu std=%zu roi=(%d,%d) calib=%s", scale_, n_mean_,
             n_std_, roi_x_, roi_y_, has_calib_ ? "yes" : "no");
    cc.LogInfo(buf);
    return lmflow::Status::Ok();
  }

  lmflow::Status Close(lmflow::Context& cc) override {
    // 只在正常排空时才认为结果可用;出错/被取消时不应提交
    if (cc.CloseReason() == LMFLOW_CLOSE_NORMAL) {
      cc.LogInfo("finished normally, result is valid");
    } else {
      cc.LogWarn("abnormal termination, discarding result");
    }
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    cc.Forward(0, 0);  // 真实实现会按 scale/mean/std 归一化,此处只示范参数获取
    return lmflow::Status::Ok();
  }

 private:
  double scale_ = 0.0;
  double mean_[4]{}, std_[4]{};
  size_t n_mean_ = 0, n_std_ = 0;
  int roi_x_ = 0, roi_y_ = 0;
  bool has_calib_ = false;
};
}  // namespace

void RegisterNormalizeKernel() {
  lmflow_register_kernel("NormalizeKernel", lmflow::KernelAdapter<NormalizeKernel>::vtable(),
                         nullptr);
}
