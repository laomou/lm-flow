// resize.cc —— OpenCL 双线性 resize 算子(注册名 "OclResize")。
//
// 与 cpp/kernels/ 的 CPU 算子同构:一个算子一个文件,`.cc` 里静态自注册,所以图里直接写
// `kernel: OclResize` 即可,无需宿主手写注册。⚠ 静态注册要求本档案以 whole-archive 链入,
// 否则链接器会把「没人引用」的目标文件整个丢掉、注册静默消失 —— CMake 已按此配置。
//
// ---- 什么时候该用它 ----
//
// 单独一个 GPU 逐元素算子是**净亏**。Adreno 840 上实测(24 MB f32):upload 8.2 ms、
// 一次 dispatch 的 GPU 执行 0.93 ms、零拷贝 download ≈ 0;而 CPU 全量走一遍同样数据
// 约 6.5 ms。所以「传上去、resize 一次、传回来」≈ 9 ms,比 CPU 直接做还慢。
// 盈亏平衡点大约在**连续 2~3 个 GPU 算子**:中间结果留在设备上不落主机时,
// 8.2 + N×0.93 才开始赢过 N×6.5。本算子的用途是充当这种链条中的一环
// (典型:resize → normalize → NCHW),不是单点替换 CPU resize。
//
// ---- 插值约定 ----
//
// 双线性,**半像素中心**(align_corners=false):`src = (dst + 0.5) * scale - 0.5`。
// 这与 OpenCV `INTER_LINEAR` 及 PyTorch `interpolate(align_corners=False)` 一致。
// 这一条必须写明:各框架此处约定不同,align_corners=true 会得到不同的像素值,
// 而"resize 结果对不上"是最难排查的那类偏差。
//
// ---- 当前限制 ----
//
// 只支持 F32、2 维 [H,W] 或 3 维 [H,W,C]。其它 dtype 明确报错而不是静默走错路。
// u8 是移动端 ISP 的常见输入,加它是机械工作(另写一份 kernel 源码按 dtype 选),
// 但那要么先 cast 到 f32、要么在 kernel 里做定点插值,取舍留给后续。
#include <string>

#include <lmflow/opencl.hpp>

namespace {

// 通道在最内维(HWC),与 LMFlowBuffer 的行优先约定一致。
// 2D NDRange 覆盖 (out_w, out_h),通道在内层循环 —— C 通常是 1~4,不值得再开一维。
const char* kResizeSource = R"CLC(
__kernel void resize_bilinear_f32(__global const float* src, __global float* dst,
                                  const int in_h, const int in_w,
                                  const int out_h, const int out_w,
                                  const int channels,
                                  const float scale_y, const float scale_x) {
  const int x = get_global_id(0);
  const int y = get_global_id(1);
  if (x >= out_w || y >= out_h) return;

  /* 半像素中心;负值钳到 0,上界钳到最后一行/列 */
  float sy = ((float)y + 0.5f) * scale_y - 0.5f;
  float sx = ((float)x + 0.5f) * scale_x - 0.5f;
  sy = fmax(sy, 0.0f);
  sx = fmax(sx, 0.0f);
  int y0 = min((int)sy, in_h - 1);
  int x0 = min((int)sx, in_w - 1);
  const int y1 = min(y0 + 1, in_h - 1);
  const int x1 = min(x0 + 1, in_w - 1);
  const float wy = sy - (float)y0;
  const float wx = sx - (float)x0;

  for (int c = 0; c < channels; ++c) {
    const float v00 = src[((long)y0 * in_w + x0) * channels + c];
    const float v01 = src[((long)y0 * in_w + x1) * channels + c];
    const float v10 = src[((long)y1 * in_w + x0) * channels + c];
    const float v11 = src[((long)y1 * in_w + x1) * channels + c];
    const float top = v00 + (v01 - v00) * wx;
    const float bottom = v10 + (v11 - v10) * wx;
    dst[((long)y * out_w + x) * channels + c] = top + (bottom - top) * wy;
  }
}
)CLC";

/// 双线性 resize:ocl::Image -> ocl::Image。注册名 "OclResize"。
///
/// 必需 option:`out_h`、`out_w`(缺失即在 Open 期失败,不静默走默认值)。
class OclResizeKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSet<lmflow::ocl::Image>(0);
    c.OutputSet<lmflow::ocl::Image>(0);
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    if (cc.RequireOption("out_h", &out_h_) != LMFLOW_OK ||
        cc.RequireOption("out_w", &out_w_) != LMFLOW_OK) {
      return cc.Fail("OclResize requires integer options out_h and out_w");
    }
    if (out_h_ <= 0 || out_w_ <= 0) {
      return cc.Fail("OclResize needs positive out_h / out_w");
    }
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet input = cc.TakeInput(0);
    const lmflow::ocl::Image* image = input.TryGet<lmflow::ocl::Image>();
    if (!image || !image->valid()) return cc.Fail("OclResize expects an ocl::Image input");
    if (image->dtype() != LMFLOW_DTYPE_F32) {
      return cc.Fail("OclResize currently supports LMFLOW_DTYPE_F32 only");
    }
    const int ndim = image->ndim();
    if (ndim != 2 && ndim != 3) {
      return cc.Fail("OclResize expects a 2D [H,W] or 3D [H,W,C] image");
    }

    const int in_h = static_cast<int>(image->shape(0));
    const int in_w = static_cast<int>(image->shape(1));
    const int channels = ndim == 3 ? static_cast<int>(image->shape(2)) : 1;
    const int out_h = static_cast<int>(out_h_);
    const int out_w = static_cast<int>(out_w_);

    int64_t out_shape[3] = {out_h, out_w, channels};
    const float scale_y = static_cast<float>(in_h) / static_cast<float>(out_h);
    const float scale_x = static_cast<float>(in_w) / static_cast<float>(out_w);

    // 输出与输入不同形,故用 DispatchSpec 指定输出形状与 2 维工作规模;加锁、绑 src/dst、
    // 等生产者、记同步点这些通用步骤都由 Enqueue 负责,算子只管自己那几个参数。
    lmflow::ocl::DispatchSpec spec;
    spec.ndim = ndim;
    spec.shape = out_shape;
    spec.work_dim = 2;
    spec.global[0] = static_cast<size_t>(out_w);
    spec.global[1] = static_cast<size_t>(out_h);

    lmflow::ocl::Image output = lmflow::ocl::Enqueue(
        *image, spec, kResizeSource, "resize_bilinear_f32", [&](cl_kernel kernel) {
          int index = 2;  // 0/1 已由 Enqueue 绑为 src/dst
          lmflow::ocl::Check(clSetKernelArg(kernel, index++, sizeof in_h, &in_h), "arg in_h");
          lmflow::ocl::Check(clSetKernelArg(kernel, index++, sizeof in_w, &in_w), "arg in_w");
          lmflow::ocl::Check(clSetKernelArg(kernel, index++, sizeof out_h, &out_h), "arg out_h");
          lmflow::ocl::Check(clSetKernelArg(kernel, index++, sizeof out_w, &out_w), "arg out_w");
          lmflow::ocl::Check(clSetKernelArg(kernel, index++, sizeof channels, &channels),
                             "arg channels");
          lmflow::ocl::Check(clSetKernelArg(kernel, index++, sizeof scale_y, &scale_y),
                             "arg scale_y");
          lmflow::ocl::Check(clSetKernelArg(kernel, index++, sizeof scale_x, &scale_x),
                             "arg scale_x");
        });

    cc.Emit(0, lmflow::Packet::Make<lmflow::ocl::Image>(std::move(output)));
    return lmflow::Status::Ok();
  }

 private:
  int64_t out_h_ = 0;
  int64_t out_w_ = 0;
};

}  // namespace

LMFLOW_REGISTER_KERNEL_AS(OclResizeKernel, "OclResize")
