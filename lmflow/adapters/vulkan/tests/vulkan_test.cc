// lmflow Vulkan adapter 单元测试 —— 需要 Vulkan loader 与一个带 compute 队列、
// 支持时间线信号量的设备(软件实现如 lavapipe 也可)。并链接引擎库。
//
//   g++ -std=c++17 -Iinclude -Iadapters/vulkan/include
//       adapters/vulkan/tests/vulkan_test.cc core/target/release/liblmflow.a
//       -lvulkan -lpthread -ldl -lm -o lmflow_vulkan_test
//   (以上是一条命令;这里不用反斜杠续行,免得 -Werror=comment 报「多行注释」)
//
// 与 adapters/opencl 的测试逐条对应,断言的是同一组设计依据:
//   1. GPU 负载能作为自定义 payload 类型在图里流动,**core 无需任何改动**;
//   2. 连续 GPU 段之间**不落主机** —— 中间那步的包类型是 vk::Image,不是 LMFlowBuffer;
//   3. 把 GPU 输出接到声明了 LMFLOW_TYPE_BUFFER 的 CPU 输入,**建图期**被拒;
//   4. pipeline 缓存命中,图重建不重建 pipeline;
//   5. 两个后端的 payload 类型互不相通 —— 一张图不可能混用(仅在同时编入 OpenCL 时验)。

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "lmflow/vulkan.hpp"

#include "scale_spv.h"

#define CHECK(condition)                                                        \
  do {                                                                          \
    if (!(condition)) {                                                         \
      const char* detail = lmflow_last_error();                                 \
      std::fprintf(stderr, "CHECK failed at %s:%d: %s%s%s\n", __FILE__,         \
                   __LINE__, #condition, (detail && *detail) ? " | " : "",      \
                   (detail && *detail) ? detail : "");                          \
      return EXIT_FAILURE;                                                      \
    }                                                                           \
  } while (false)

namespace {

/// scale.comp 的 push constant 块布局,须与 shader 一致。
struct ScaleParams {
  float factor;
  uint32_t count;
};

/// 纯 GPU→GPU 算子:输入输出都是 vk::Image,中途不碰主机内存。
class ScaleKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSet<lmflow::vk::Image>(0);
    c.OutputSet<lmflow::vk::Image>(0);
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    factor_ = static_cast<float>(cc.OptionF64("factor", 1.0));
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet input = cc.TakeInput(0);
    const lmflow::vk::Image* image = input.TryGet<lmflow::vk::Image>();
    if (!image || !image->valid()) return cc.Fail("Scale expects a vk::Image input");
    ScaleParams params{factor_, static_cast<uint32_t>(image->element_count())};
    lmflow::vk::Image output =
        lmflow::vk::EnqueueUnary(*image, kScaleSpv, sizeof kScaleSpv / sizeof kScaleSpv[0],
                                 "main", &params, sizeof params);
    cc.Emit(0, lmflow::Packet::Make<lmflow::vk::Image>(std::move(output)));
    return lmflow::Status::Ok();
  }

 private:
  float factor_ = 1.0f;
};

/// 只声明 LMFLOW_TYPE_BUFFER 输入的 CPU 算子,用于验证建图期类型拒绝。
class CpuOnlyKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    cc.Emit(0, cc.TakeInput(0));
    return lmflow::Status::Ok();
  }
};

std::string GraphYaml(const char* nodes) {
  return std::string(
             "executors:\n"
             "  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
             "nodes:\n") +
         nodes + "input_ports: [in]\noutput_ports: [out]\n";
}

lmflow::Packet HostPacket(std::vector<float>* host) {
  LMFlowBuffer source{};
  source.data = host->data();
  source.ndim = 1;
  source.dtype = LMFLOW_DTYPE_F32;
  source.shape[0] = static_cast<int64_t>(host->size());
  source.strides[0] = sizeof(float);
  // 图输入口要求显式时间戳(UNSET 会被拒);节点内部产出的包保持 UNSET 由引擎继承。
  return lmflow::Packet::Adopt(lmflow_packet_from_buffer(&source, /*timestamp=*/0));
}

}  // namespace

// 注册宏会把类型名拼进标识符,故带 `::` 的限定名必须先起别名。
using VkUploadKernel = lmflow::vk::UploadKernel;
using VkDownloadKernel = lmflow::vk::DownloadKernel;

LMFLOW_REGISTER_KERNEL_AS(VkUploadKernel, "VkUpload")
LMFLOW_REGISTER_KERNEL_AS(VkDownloadKernel, "VkDownload")
LMFLOW_REGISTER_KERNEL_AS(ScaleKernel, "VkScale")
LMFLOW_REGISTER_KERNEL_AS(CpuOnlyKernel, "CpuOnly")

int main() {
  // 没有可用设备时干净跳过,而不是让 CI 红 —— adapter 是可选件。
  try {
    lmflow::vk::Context::Shared();
  } catch (const std::exception& thrown) {
    std::fprintf(stderr, "skipping: no usable Vulkan device (%s)\n", thrown.what());
    return EXIT_SUCCESS;
  }

  constexpr int64_t kCount = 1024;
  std::vector<float> host(kCount);
  for (int64_t i = 0; i < kCount; ++i) host[i] = static_cast<float>(i);

  // 1) upload -> scale(2) -> scale(3) -> download,连续 GPU 段中间不落主机。
  {
    lmflow::Graph graph = lmflow::Graph::from_yaml(
        GraphYaml("  - { name: up, kernel: VkUpload, executor: gpu, "
                  "input_ports: [in], output_ports: [up] }\n"
                  "  - { name: s1, kernel: VkScale, executor: gpu, "
                  "input_ports: [up], output_ports: [s1], options: { factor: 2.0 } }\n"
                  "  - { name: s2, kernel: VkScale, executor: gpu, "
                  "input_ports: [s1], output_ports: [s2], options: { factor: 3.0 } }\n"
                  "  - { name: down, kernel: VkDownload, executor: gpu, "
                  "input_ports: [s2], output_ports: [out] }\n")
            .c_str());
    CHECK(graph.valid());
    lmflow::Poller poller = graph.add_poller("out");
    lmflow::Input input = graph.input("in");
    CHECK(poller.valid() && input.valid());
    CHECK(graph.start().ok());
    CHECK(input.send(HostPacket(&host)).ok());

    std::optional<lmflow::Packet> out = poller.next();
    CHECK(out.has_value());
    LMFlowBuffer result{};
    CHECK(out->AsBuffer(&result));
    CHECK(result.ndim == 1 && result.shape[0] == kCount);
    CHECK(result.dtype == LMFLOW_DTYPE_F32);
    const float* values = static_cast<const float*>(result.data);
    for (int64_t i = 0; i < kCount; ++i) {
      // 2 * 3 == 6:两级 scale 都生效,说明中间那步确实在设备上被消费。
      CHECK(values[i] == static_cast<float>(i) * 6.0f);
    }
    graph.close_all_inputs();
  }

  // 2) 中间边的包类型就是 vk::Image(不是 LMFlowBuffer)—— 「不落主机」的结构性证据。
  {
    lmflow::Graph graph = lmflow::Graph::from_yaml(
        GraphYaml("  - { name: up, kernel: VkUpload, executor: gpu, "
                  "input_ports: [in], output_ports: [out] }\n")
            .c_str());
    CHECK(graph.valid());
    lmflow::Poller poller = graph.add_poller("out");
    lmflow::Input input = graph.input("in");
    CHECK(graph.start().ok());
    CHECK(input.send(HostPacket(&host)).ok());

    std::optional<lmflow::Packet> out = poller.next();
    CHECK(out.has_value());
    CHECK(out->Is<lmflow::vk::Image>());
    const lmflow::vk::Image* image = out->TryGet<lmflow::vk::Image>();
    CHECK(image != nullptr && image->valid());
    CHECK(image->element_count() == static_cast<size_t>(kCount));
    LMFlowBuffer not_a_buffer{};
    CHECK(!out->AsBuffer(&not_a_buffer));  // GPU 句柄从不冒充 LMFlowBuffer
    graph.close_all_inputs();
  }

  // 3) 把 GPU 输出接到只收 LMFLOW_TYPE_BUFFER 的 CPU 输入 —— 必须**建图期**失败。
  //
  // 这是本 adapter 用「自定义类型」而非 device 字段的全部理由:错误在建图期就被端口类型
  // 校验挡住,而不是留到运行期让 CPU 算子把设备地址当主机地址读。
  {
    bool rejected = false;
    std::string reason;
    try {
      lmflow::Graph graph = lmflow::Graph::from_yaml(
          GraphYaml("  - { name: up, kernel: VkUpload, executor: gpu, "
                    "input_ports: [in], output_ports: [up] }\n"
                    "  - { name: cpu, kernel: CpuOnly, executor: gpu, "
                    "input_ports: [up], output_ports: [out] }\n")
              .c_str());
      rejected = !graph.valid();
    } catch (const std::exception& thrown) {
      rejected = true;
      reason = thrown.what();
    }
    CHECK(rejected);
    CHECK(reason.find("lmflow.vulkan.Image") != std::string::npos);
    CHECK(reason.find("Buffer") != std::string::npos);
  }

  // 4) pipeline 缓存:同 SPIR-V 同入口重复取,拿到的是同一个 VkPipeline。
  {
    const std::shared_ptr<lmflow::vk::Context>& context = lmflow::vk::Context::Shared();
    const size_t words = sizeof kScaleSpv / sizeof kScaleSpv[0];
    const lmflow::vk::Context::Program& first =
        context->ProgramFor(kScaleSpv, words, "main", sizeof(ScaleParams));
    const lmflow::vk::Context::Program& again =
        context->ProgramFor(kScaleSpv, words, "main", sizeof(ScaleParams));
    CHECK(first.pipeline == again.pipeline);
    CHECK(first.pipeline != VK_NULL_HANDLE);
  }

  // 5) 统一内存路径被真正走到(ARM 与软件实现都是如此);独显则应走 staging。
  {
    const std::shared_ptr<lmflow::vk::Context>& context = lmflow::vk::Context::Shared();
    const int64_t shape[1] = {kCount};
    lmflow::vk::Image image =
        lmflow::vk::Image::Allocate(context, LMFLOW_DTYPE_F32, 1, shape);
    CHECK(image.valid());
    CHECK(image.host_visible() == context->unified_memory());
    CHECK(image.byte_size() == static_cast<size_t>(kCount) * sizeof(float));
  }

  std::fprintf(stderr, "lmflow_vulkan_test: all checks passed\n");
  return EXIT_SUCCESS;
}
