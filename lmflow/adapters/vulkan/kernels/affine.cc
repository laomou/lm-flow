// affine.cc —— Vulkan 逐元素仿射算子(注册名 "VkAffine")。
//
// 与 adapters/opencl/kernels/affine.cc **同构**,也与 cpp/kernels/affine.cc 的 CPU 版
// 语义一致:out = in * scale + shift。一个算子一个文件、`.cc` 里静态自注册,图里直接写
// `kernel: VkAffine`。⚠ 静态注册要求本档案以 whole-archive 链入,CMake 已配置。
//
// 什么时候该用:单个 GPU 逐元素算子是净亏(见 resize.cc 的实测数字)。affine 的价值在于
// **接在别的 GPU 算子后面、中间结果不落主机** —— 图像预处理里 resize → affine(归一化)
// 是最常见的一对,这也是本 adapter 缓冲池化(设备 buffer 跨 dispatch 复用)真正开始回本的
// 场景。本算子就是为「连续 GPU 段」提供的第二个环。
//
// 限制:F32(与 VkResize 一致)。dtype 转换交给 CPU cast,GPU 侧只做同 dtype 的数值变换。
#include <cstdint>
#include <lmflow/vulkan.hpp>

#include "affine_spv.h"

namespace {

/// 与 shaders/affine.comp 的 push_constant 块**逐字段对应**,顺序与类型不可动。
struct AffineParams {
  float scale;
  float shift;
  uint32_t count;
};

/// 逐元素仿射:vk::Image -> vk::Image(同形同 dtype)。注册名 "VkAffine"。
///
/// option:`scale`(默认 1)、`shift`(默认 0) —— 与 CPU affine 同名同义。
class VkAffineKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSet<lmflow::vk::Image>(0);
    c.OutputSet<lmflow::vk::Image>(0);
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    scale_ = static_cast<float>(cc.OptionF64("scale", 1.0));
    shift_ = static_cast<float>(cc.OptionF64("shift", 0.0));
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet input = cc.TakeInput(0);
    const lmflow::vk::Image* image = input.TryGet<lmflow::vk::Image>();
    if (!image || !image->valid()) return cc.Fail("VkAffine expects a vk::Image input");
    if (image->dtype() != LMFLOW_DTYPE_F32) {
      return cc.Fail("VkAffine currently supports LMFLOW_DTYPE_F32 only");
    }

    AffineParams params{};
    params.scale = scale_;
    params.shift = shift_;
    params.count = static_cast<uint32_t>(image->element_count());

    // 同形逐元素:EnqueueUnary 输出与输入同形同类型、按元素数铺 1 维(local_size_x = 64,
    // 与 affine.comp 一致)。分配 / 绑 binding 0-1 / 推 push constant / 按生产者时间线值提交 /
    // 记同步点与延迟回收都由它负责。
    lmflow::vk::Image output = lmflow::vk::EnqueueUnary(
        *image, kAffineSpv, sizeof kAffineSpv / sizeof kAffineSpv[0], "main", &params,
        sizeof params);

    cc.Emit(0, lmflow::Packet::Make<lmflow::vk::Image>(std::move(output)));
    return lmflow::Status::Ok();
  }

 private:
  float scale_ = 1.0f;
  float shift_ = 0.0f;
};

}  // namespace

LMFLOW_REGISTER_KERNEL_AS(VkAffineKernel, "VkAffine")
