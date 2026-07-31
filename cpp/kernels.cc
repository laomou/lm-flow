/*
 * kernels.cc —— 内置示例算子集。既是可用算子,也是 API 覆盖用例。
 *
 *  算子              用途                        覆盖的接口
 *  ----------------  --------------------------  ------------------------------
 *  PassThrough       零拷贝直通                  Forward
 *  Scale             参数化数值变换              options(OptionI64)、类型声明
 *  Sum               有状态累加,Close 时输出总和  跨包状态、Open/Close、PostStream
 *  Split             1 进 2 出(扇出)            多输出、Forward 到多个口
 *  Zip               2 进 1 出(按 tag 取端口)    多输入、InputId(tag)、类型混合
 *  Filter            条件过滤(不产出即推进边界)  不 Emit + SetNextTimestampBound
 *  Stringify         类型转换 int -> std::string  异类型输入输出
 *  Sink              只消费不产出                零输出口
 *  Invert            原地改写(省拷贝)            TakeInput + CoW MakeMutableBuffer
 *  Normalize         参数化归一化                  必需参数 / 数组参数 / 点号路径 / side packet
 *
 * 类型约定:捆绑算子一律用**内建类型**(FLOW_TYPE_I64 等)而非 C++ 原生 typeid ——
 * 这样它们从 C++、Rust、Python 三侧都能直接使用。若改用 InputSet<int>,
 * Python 送来的整数(内建 I64)就会被类型校验拒绝。
 *
 * 注册方式:本文件用**显式聚合注册**(flow_register_builtin_kernels),
 * 因为静态初始化对象在静态库中可能被链接器裁剪。用户自己的算子可以直接用
 * FLOW_REGISTER_KERNEL 宏(更省事),见 flow.hpp。
 */
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>

#include "flow.hpp"

namespace {

/* ---------- 1. 直通:零拷贝转发 ---------- */
class PassThroughKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
  }
  flow::Status Process(flow::Context& cc) override {
    cc.Forward(0, 0);  // 复用同一 payload,不拷贝
    return flow::Status::Ok();
  }
};

/* ---------- 2. 数值变换:读 options ---------- */
class ScaleKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetBuiltin(0, FLOW_TYPE_I64);
    c.OutputSetBuiltin(0, FLOW_TYPE_I64);
  }
  flow::Status Open(flow::Context& cc) override {
    factor_ = cc.OptionI64("factor", 1);
    return flow::Status::Ok();
  }
  flow::Status Process(flow::Context& cc) override {
    int64_t v = 0;
    if (!cc.Input(0).AsI64(&v)) return cc.Fail("输入不是整数包");
    cc.Emit(0, flow::Packet::FromI64(v * factor_));
    return flow::Status::Ok();
  }

 private:
  int64_t factor_ = 1;
};

/* ---------- 3. 有状态累加:Close 时吐出总和 ---------- */
class SumKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetBuiltin(0, FLOW_TYPE_I64);
    c.OutputSetBuiltin(0, FLOW_TYPE_I64);
  }
  flow::Status Open(flow::Context&) override {
    total_ = 0;
    return flow::Status::Ok();
  }
  flow::Status Process(flow::Context& cc) override {
    int64_t v = 0;
    if (cc.Input(0).AsI64(&v)) total_ += v;
    return flow::Status::Ok();  // 中途不产出
  }
  flow::Status Close(flow::Context& cc) override {
    // 流尾单包位置:表示「整条流结束时的一个汇总结果」
    cc.Emit(0, flow::Packet::FromI64(total_).At(FLOW_TS_POST_STREAM));
    return flow::Status::Ok();
  }

 private:
  int64_t total_ = 0;
};

/* ---------- 4. 扇出:1 进 2 出 ---------- */
class SplitKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetAny(0);
    for (size_t i = 0; i < c.NumOutputs(); ++i) c.OutputSetAny(i);
  }
  flow::Status Process(flow::Context& cc) override {
    for (size_t i = 0; i < cc.NumOutputs(); ++i) cc.Forward(0, i);  // 共享同一 payload
    return flow::Status::Ok();
  }
};

/* ---------- 5. 汇合:2 进 1 出,按 tag 定位端口 ---------- */
class ZipKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetBuiltin(0, FLOW_TYPE_I64);
    c.InputSetBuiltin(1, FLOW_TYPE_I64);
    c.OutputSetBuiltin(0, FLOW_TYPE_I64);
  }
  flow::Status Open(flow::Context& cc) override {
    // 按 tag 定位,不依赖 YAML 书写顺序 —— 端口声明形如:
    //     input_ports: ["A:left_stream", "B:right_stream"]
    // 于是不管两者谁先写、边名叫什么,LHS 永远是 tag 为 A 的那个口。
    lhs_ = cc.InputId("A");
    rhs_ = cc.InputId("B");
    // 若 YAML 没写 tag(input_ports: ["x","y"]),退回按声明顺序取序号 0/1。
    if (lhs_ == FLOW_INVALID_ID) lhs_ = 0;
    if (rhs_ == FLOW_INVALID_ID) rhs_ = 1;
    return flow::Status::Ok();
  }
  flow::Status Process(flow::Context& cc) override {
    int64_t a = 0, b = 0;
    // 时间戳对齐后某口仍可能无数据(该时刻它就是没有)—— 这时不产出
    if (!cc.Input(lhs_).AsI64(&a) || !cc.Input(rhs_).AsI64(&b)) {
      return flow::Status::Ok();
    }
    cc.Emit(0, flow::Packet::FromI64(a + b));
    return flow::Status::Ok();
  }

 private:
  size_t lhs_ = 0, rhs_ = 1;
};

/* ---------- 6. 过滤:不产出时推进时间戳边界 ---------- */
class FilterKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetBuiltin(0, FLOW_TYPE_I64);
    c.OutputSetBuiltin(0, FLOW_TYPE_I64);
  }
  flow::Status Open(flow::Context& cc) override {
    threshold_ = cc.OptionI64("threshold", 0);
    return flow::Status::Ok();
  }
  flow::Status Process(flow::Context& cc) override {
    int64_t v = 0;
    if (!cc.Input(0).AsI64(&v)) return cc.Fail("输入不是整数包");
    if (v >= threshold_) {
      cc.Forward(0, 0);
    } else {
      // 丢弃该包。必须告知下游「此刻之前不会再有数据」,否则下游会一直等。
      cc.SetNextTimestampBound(0, cc.InputTimestamp() + 1);
    }
    return flow::Status::Ok();
  }

 private:
  int64_t threshold_ = 0;
};

/* ---------- 7. 类型转换:int -> std::string ---------- */
class StringifyKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetBuiltin(0, FLOW_TYPE_I64);
    c.OutputSetBuiltin(0, FLOW_TYPE_STR);
  }
  flow::Status Process(flow::Context& cc) override {
    int64_t v = 0;
    if (!cc.Input(0).AsI64(&v)) return cc.Fail("输入不是整数包");
    cc.Emit(0, flow::Packet::FromStr(std::to_string(v).c_str()));
    return flow::Status::Ok();
  }
};

/* ---------- 8. 汇点:只消费,无输出口 ---------- */
class SinkKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) { c.InputSetAny(0); }
  flow::Status Process(flow::Context& cc) override {
    // 走引擎日志而非 printf:库不该抢占宿主的 stdout
    char buf[64];
    snprintf(buf, sizeof(buf), "收到包 @ ts=%lld",
             static_cast<long long>(cc.InputTimestamp()));
    cc.Log(FLOW_LOG_DEBUG, buf);
    cc.CounterAdd("sink.packets");
    ++count_;
    return flow::Status::Ok();
  }
  flow::Status Close(flow::Context& cc) override {
    char buf[64];
    snprintf(buf, sizeof(buf), "共处理 %lld 个包", static_cast<long long>(count_));
    cc.LogInfo(buf);
    // 计数器是**按图**的,比全局日志更适合被测试断言
    cc.CounterAdd("sink.closed");
    return flow::Status::Ok();
  }

 private:
  long long count_ = 0;
};

/* ---------- 9. 原地改写:CoW 省拷贝路径的示范 ----------
 * 线性管线上(本节点是唯一消费者)全程零拷贝;若上游是 Split 扇出、
 * payload 被别的分支共享,MakeMutableBuffer 才会复制一份,保证不污染对方。 */
class InvertKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
  }
  flow::Status Process(flow::Context& cc) override {
    // 关键第一步:把包从输入槽**取走**。否则上下文仍持一份引用,CoW 必然复制。
    flow::Packet p = cc.TakeInput(0);
    FlowBuffer buf{};
    if (FlowStatus st = p.MakeMutableBuffer(&buf)) return st;
    if (buf.dtype != FLOW_DTYPE_U8 || buf.ndim < 2) return FLOW_ERR_INVALID_ARG;

    const size_t row_bytes = static_cast<size_t>(buf.shape[1]) *
                             (buf.ndim >= 3 ? static_cast<size_t>(buf.shape[2]) : 1);
    for (int64_t y = 0; y < buf.shape[0]; ++y) {
      auto* line = static_cast<uint8_t*>(buf.data) + y * buf.strides[0];
      for (size_t x = 0; x < row_bytes; ++x) line[x] = static_cast<uint8_t>(255 - line[x]);
    }
    cc.Emit(0, std::move(p));
    return flow::Status::Ok();
  }
};

/* ---------- 10. 参数用法示范 ----------
 * YAML:
 *   - name: "norm"
 *     kernel: "NormalizeKernel"
 *     input_ports: ["in"]
 *     output_ports: ["out"]
 *     options:
 *       scale: 0.00392156862   # 必需:缺了就在 Open 阶段直接失败
 *       mean:  [0.485, 0.456, 0.406]
 *       std:   [0.229, 0.224, 0.225]
 *       roi:   { x: 8, y: 8 }  # 嵌套:用点号路径读
 */
class NormalizeKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
    // 声明必需的 side packet:宿主漏注入 → init 阶段就报错,不必等到 open
    c.RequireSidePacket("calibration");
  }

  flow::Status Open(flow::Context& cc) override {
    // 必需参数:拼错 key 或漏配会当场失败,并带上可读原因(不是静默用默认值跑歪)
    if (cc.RequireOption("scale", &scale_) != FLOW_OK) {
      return cc.Fail("options.scale 缺失或类型不符(必需参数)");
    }

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
    return flow::Status::Ok();
  }

  flow::Status Close(flow::Context& cc) override {
    // 只在正常排空时才认为结果可用;出错/被取消时不应提交
    if (cc.CloseReason() == FLOW_CLOSE_NORMAL) {
      cc.LogInfo("正常结束,结果有效");
    } else {
      cc.LogWarn("异常结束,丢弃结果");
    }
    return flow::Status::Ok();
  }

  flow::Status Process(flow::Context& cc) override {
    cc.Forward(0, 0);  // 真实实现会按 scale/mean/std 归一化,此处只示范参数获取
    return flow::Status::Ok();
  }

 private:
  double scale_ = 0.0;
  double mean_[4]{}, std_[4]{};
  size_t n_mean_ = 0, n_std_ = 0;
  int roi_x_ = 0, roi_y_ = 0;
  bool has_calib_ = false;
};

}  // namespace

/*
 * 显式聚合注册。宿主(Rust 侧 flow_core::register_builtin_kernels())调用一次。
 * 相比静态初始化,这条路径不会被链接器裁剪。
 */
extern "C" void flow_register_builtin_kernels(void) {
#define FLOW_REG(T) flow_register_kernel(#T, flow::KernelAdapter<T>::vtable(), nullptr)
  FLOW_REG(PassThroughKernel);
  FLOW_REG(ScaleKernel);
  FLOW_REG(SumKernel);
  FLOW_REG(SplitKernel);
  FLOW_REG(ZipKernel);
  FLOW_REG(FilterKernel);
  FLOW_REG(StringifyKernel);
  FLOW_REG(SinkKernel);
  FLOW_REG(InvertKernel);
  FLOW_REG(NormalizeKernel);
#undef FLOW_REG
}
