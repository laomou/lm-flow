// affine.cc —— OpenCL 逐元素仿射算子(注册名 "OclAffine")。
//
// 与 adapters/vulkan/kernels/affine.cc **同构**,也与 cpp/kernels/affine.cc 的 CPU 版
// 语义一致:out = in * scale + shift。一个算子一个文件、`.cc` 里静态自注册,图里直接写
// `kernel: OclAffine`。⚠ 静态注册要求本档案以 whole-archive 链入,CMake 已配置。
//
// 什么时候该用:单个 GPU 逐元素算子净亏(见 resize.cc 的实测数字)。affine 的价值在于
// 接在别的 GPU 算子后面、中间结果不落主机 —— 图像预处理里 resize → affine(归一化)是最
// 常见的一对,也是本 adapter 缓冲池化真正开始回本的场景。它是「连续 GPU 段」的第二个环。
//
// 限制:F32(与 OclResize 一致)。dtype 转换交给 CPU cast,GPU 侧只做同 dtype 的数值变换。
#include <string>

#include <lmflow/opencl.hpp>

namespace {

// 逐元素,无需 count/边界检查:Enqueue 用精确的 global = 输出元素数、local 交给 runtime
// (local_work_size = NULL),故 get_global_id(0) 恒落在 [0, count) 内,不会越界。
// (Vulkan 侧按固定 local_size = 64 铺、会向上取整,才需要 count 守卫。)
const char* kAffineSource = R"CLC(
__kernel void affine_f32(__global const float* src, __global float* dst,
                         const float scale, const float shift) {
  const size_t i = get_global_id(0);
  dst[i] = src[i] * scale + shift;
}
)CLC";

/// 逐元素仿射:ocl::Image -> ocl::Image(同形同 dtype)。注册名 "OclAffine"。
///
/// option:`scale`(默认 1)、`shift`(默认 0) —— 与 CPU affine 同名同义。
class OclAffineKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSet<lmflow::ocl::Image>(0);
    c.OutputSet<lmflow::ocl::Image>(0);
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    scale_ = static_cast<float>(cc.OptionF64("scale", 1.0));
    shift_ = static_cast<float>(cc.OptionF64("shift", 0.0));
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet input = cc.TakeInput(0);
    const lmflow::ocl::Image* image = input.TryGet<lmflow::ocl::Image>();
    if (!image || !image->valid()) return cc.Fail("OclAffine expects an ocl::Image input");
    if (image->dtype() != LMFLOW_DTYPE_F32) {
      return cc.Fail("OclAffine currently supports LMFLOW_DTYPE_F32 only");
    }

    const float scale = scale_;
    const float shift = shift_;

    // 同形逐元素:spec 全默认(输出与输入同形同 dtype、按元素数铺 1 维)。加锁、绑 src/dst、
    // 等生产者、记同步点都由 Enqueue 负责;0/1 参已绑为 src/dst,算子只补 scale/shift。
    lmflow::ocl::Image output = lmflow::ocl::EnqueueUnary(
        *image, kAffineSource, "affine_f32", [&](cl_kernel kernel) {
          lmflow::ocl::Check(clSetKernelArg(kernel, 2, sizeof scale, &scale), "arg scale");
          lmflow::ocl::Check(clSetKernelArg(kernel, 3, sizeof shift, &shift), "arg shift");
        });

    cc.Emit(0, lmflow::Packet::Make<lmflow::ocl::Image>(std::move(output)));
    return lmflow::Status::Ok();
  }

 private:
  float scale_ = 1.0f;
  float shift_ = 0.0f;
};

}  // namespace

LMFLOW_REGISTER_KERNEL_AS(OclAffineKernel, "OclAffine")
