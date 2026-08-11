// resize.cc —— Vulkan 双线性 resize 算子(注册名 "VkResize")。
//
// 与 adapters/opencl/kernels/resize.cc **同构**:一个算子一个文件、`.cc` 里静态自注册,
// 图里直接写 `kernel: VkResize`。⚠ 静态注册要求本档案以 whole-archive 链入,CMake 已配置。
//
// 插值约定与 OpenCL 侧、与 shaders/resize.comp 三处必须一致:半像素中心
// (align_corners=false),对齐 OpenCV INTER_LINEAR / PyTorch 默认。改一处就要改三处,
// 否则两个后端出的图差半个像素 —— 看起来"正常",最难排查。
//
// 什么时候该用:单个 GPU 逐元素算子是净亏(Adreno 840 / 24 MB f32 实测:upload 8.2 ms、
// 一次 dispatch 的 GPU 执行 0.93 ms、零拷贝 download ≈ 0,而 CPU 全量走一遍约 6.5 ms)。
// 盈亏平衡在**连续 2~3 个 GPU 算子**,中间结果不落主机时才赢。本算子是链条中的一环。
//
// 限制:F32、2 维 [H,W] 或 3 维 [H,W,C]。其它 dtype 明确报错,不静默走错路。
#include <cstring>
#include <memory>
#include <mutex>

#include <lmflow/vulkan.hpp>

#include "resize_spv.h"

namespace {

/// 与 shaders/resize.comp 的 push_constant 块**逐字段对应**,顺序与类型不可动。
struct ResizeParams {
  int32_t in_h;
  int32_t in_w;
  int32_t out_h;
  int32_t out_w;
  int32_t channels;
  float scale_y;
  float scale_x;
};

/// 与 shader 的 local_size 必须一致 —— 不一致会导致覆盖不全或越界读。
constexpr uint32_t kLocalSize = 8;

/// 双线性 resize:vk::Image -> vk::Image。注册名 "VkResize"。
///
/// 必需 option:`out_h`、`out_w`(缺失即在 Open 期失败,不静默走默认值)。
class VkResizeKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSet<lmflow::vk::Image>(0);
    c.OutputSet<lmflow::vk::Image>(0);
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    if (cc.RequireOption("out_h", &out_h_) != LMFLOW_OK ||
        cc.RequireOption("out_w", &out_w_) != LMFLOW_OK) {
      return cc.Fail("VkResize requires integer options out_h and out_w");
    }
    if (out_h_ <= 0 || out_w_ <= 0) {
      return cc.Fail("VkResize needs positive out_h / out_w");
    }
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet input = cc.TakeInput(0);
    const lmflow::vk::Image* image = input.TryGet<lmflow::vk::Image>();
    if (!image || !image->valid()) return cc.Fail("VkResize expects a vk::Image input");
    if (image->dtype() != LMFLOW_DTYPE_F32) {
      return cc.Fail("VkResize currently supports LMFLOW_DTYPE_F32 only");
    }
    const int ndim = image->ndim();
    if (ndim != 2 && ndim != 3) {
      return cc.Fail("VkResize expects a 2D [H,W] or 3D [H,W,C] image");
    }

    ResizeParams params{};
    params.in_h = static_cast<int32_t>(image->shape(0));
    params.in_w = static_cast<int32_t>(image->shape(1));
    params.out_h = static_cast<int32_t>(out_h_);
    params.out_w = static_cast<int32_t>(out_w_);
    params.channels = ndim == 3 ? static_cast<int32_t>(image->shape(2)) : 1;
    params.scale_y = static_cast<float>(params.in_h) / static_cast<float>(params.out_h);
    params.scale_x = static_cast<float>(params.in_w) / static_cast<float>(params.out_w);

    int64_t out_shape[3] = {params.out_h, params.out_w, params.channels};
    const std::shared_ptr<lmflow::vk::Context>& context = image->context();
    lmflow::vk::Image output =
        lmflow::vk::Image::Allocate(context, LMFLOW_DTYPE_F32, ndim, out_shape);

    // 输出与输入不同形,故用不了 EnqueueUnary(它按输入形状分配输出);这里自己录制
    // 一次 2D dispatch。ProgramFor 建的 descriptor layout 正好是两个 storage buffer,
    // 与 shader 的 binding 0/1 对应,所以能直接复用、不必自建 layout。
    const lmflow::vk::Context::Program& program =
        context->ProgramFor(kResizeSpv, sizeof kResizeSpv / sizeof kResizeSpv[0], "main",
                            sizeof params);
    const uint32_t groups_x = (static_cast<uint32_t>(params.out_w) + kLocalSize - 1) / kLocalSize;
    const uint32_t groups_y = (static_cast<uint32_t>(params.out_h) + kLocalSize - 1) / kLocalSize;

    uint64_t signalled = 0;
    VkCommandBuffer command_buffer = VK_NULL_HANDLE;
    VkDescriptorSet descriptor_set = VK_NULL_HANDLE;
    {
      std::lock_guard<std::mutex> guard(context->submit_mutex());
      context->ReclaimCompletedLocked();
      context->AllocateLocked(program.set_layout, &command_buffer, &descriptor_set);

      VkDescriptorBufferInfo buffers[2]{};
      buffers[0].buffer = image->buffer();
      buffers[0].range = VK_WHOLE_SIZE;
      buffers[1].buffer = output.buffer();
      buffers[1].range = VK_WHOLE_SIZE;
      VkWriteDescriptorSet writes[2]{};
      for (uint32_t i = 0; i < 2; ++i) {
        writes[i].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
        writes[i].dstSet = descriptor_set;
        writes[i].dstBinding = i;
        writes[i].descriptorCount = 1;
        writes[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        writes[i].pBufferInfo = &buffers[i];
      }
      vkUpdateDescriptorSets(context->device(), 2, writes, 0, nullptr);

      VkCommandBufferBeginInfo begin{};
      begin.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
      begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
      lmflow::vk::Check(vkBeginCommandBuffer(command_buffer, &begin), "vkBeginCommandBuffer");
      vkCmdBindPipeline(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE, program.pipeline);
      vkCmdBindDescriptorSets(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE,
                              program.pipeline_layout, 0, 1, &descriptor_set, 0, nullptr);
      vkCmdPushConstants(command_buffer, program.pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0,
                         sizeof params, &params);
      vkCmdDispatch(command_buffer, groups_x, groups_y, 1);
      lmflow::vk::Check(vkEndCommandBuffer(command_buffer), "vkEndCommandBuffer");

      // 以生产者的时间线值为等待值 —— 设备侧串联,CPU 线程不阻塞。
      signalled = context->SubmitLocked(command_buffer, image->ready());
    }
    output.SetReady(signalled);
    output.RetainDispatch(command_buffer, descriptor_set);

    cc.Emit(0, lmflow::Packet::Make<lmflow::vk::Image>(std::move(output)));
    return lmflow::Status::Ok();
  }

 private:
  int64_t out_h_ = 0;
  int64_t out_w_ = 0;
};

}  // namespace

LMFLOW_REGISTER_KERNEL_AS(VkResizeKernel, "VkResize")
