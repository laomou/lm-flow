// lmflow OpenCL adapter 单元测试 —— 需要 OpenCL 运行时与一个可用设备,并链接引擎库。
//
//   g++ -std=c++17 -Iinclude -Iadapters/opencl/include
//       adapters/opencl/tests/opencl_test.cc core/target/release/liblmflow.a
//       -lOpenCL -lpthread -ldl -lm -o lmflow_opencl_test
//   (以上是一条命令;这里不用反斜杠续行,免得 -Werror=comment 报「多行注释」)
//
// 本测试要证明三件事,它们正是 adapter 的设计依据:
//   1. GPU 负载能作为自定义 payload 类型在图里流动,**core 无需任何改动**;
//   2. 连续的 GPU 段之间**不落主机** —— 中间那一步的包类型是 ocl::Image,不是 LMFlowBuffer;
//   3. 把 GPU 输出接到声明了 LMFLOW_TYPE_BUFFER 的 CPU 输入,会在**建图期**被拒绝。

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "lmflow/opencl.hpp"

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

const char* kScaleSource = R"CLC(
__kernel void scale(__global const float* in, __global float* out, const float factor) {
  const size_t i = get_global_id(0);
  out[i] = in[i] * factor;
}
)CLC";

/// 纯 GPU→GPU 算子:输入输出都是 ocl::Image,中途不碰主机内存。
class ScaleKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSet<lmflow::ocl::Image>(0);
    c.OutputSet<lmflow::ocl::Image>(0);
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    factor_ = static_cast<float>(cc.OptionF64("factor", 1.0));
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet input = cc.TakeInput(0);
    const lmflow::ocl::Image* image = input.TryGet<lmflow::ocl::Image>();
    if (!image || !image->valid()) return cc.Fail("Scale expects an ocl::Image input");
    const float factor = factor_;
    lmflow::ocl::Image output = lmflow::ocl::EnqueueUnary(
        *image, kScaleSource, "scale",
        [factor](cl_kernel kernel) {
          lmflow::ocl::Check(clSetKernelArg(kernel, 2, sizeof factor, &factor),
                             "clSetKernelArg(2)");
        });
    cc.Emit(0, lmflow::Packet::Make<lmflow::ocl::Image>(std::move(output)));
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

}  // namespace

// 注册宏会把类型名拼进标识符,故带 `::` 的限定名必须先起别名。
using OclUploadKernel = lmflow::ocl::UploadKernel;
using OclDownloadKernel = lmflow::ocl::DownloadKernel;

LMFLOW_REGISTER_KERNEL_AS(OclUploadKernel, "OclUpload")
LMFLOW_REGISTER_KERNEL_AS(OclDownloadKernel, "OclDownload")
LMFLOW_REGISTER_KERNEL_AS(ScaleKernel, "OclScale")
LMFLOW_REGISTER_KERNEL_AS(CpuOnlyKernel, "CpuOnly")

int main() {
  // 没有 OpenCL 设备时干净跳过,而不是让 CI 红 —— adapter 是可选件。
  try {
    lmflow::ocl::Context::Shared();
  } catch (const std::exception& thrown) {
    std::fprintf(stderr, "skipping: no usable OpenCL device (%s)\n", thrown.what());
    return EXIT_SUCCESS;
  }

  constexpr int64_t kCount = 1024;
  std::vector<float> host(kCount);
  for (int64_t i = 0; i < kCount; ++i) host[i] = static_cast<float>(i);

  // 1) upload -> scale(2) -> scale(3) -> download,连续 GPU 段中间不落主机。
  {
    lmflow::Graph graph = lmflow::Graph::from_yaml(
        GraphYaml("  - { name: up, kernel: OclUpload, executor: gpu, "
                  "input_ports: [in], output_ports: [up] }\n"
                  "  - { name: s1, kernel: OclScale, executor: gpu, "
                  "input_ports: [up], output_ports: [s1], options: { factor: 2.0 } }\n"
                  "  - { name: s2, kernel: OclScale, executor: gpu, "
                  "input_ports: [s1], output_ports: [s2], options: { factor: 3.0 } }\n"
                  "  - { name: down, kernel: OclDownload, executor: gpu, "
                  "input_ports: [s2], output_ports: [out] }\n")
            .c_str());
    CHECK(graph.valid());
    lmflow::Poller poller = graph.add_poller("out");
    lmflow::Input input = graph.input("in");
    CHECK(poller.valid() && input.valid());
    CHECK(graph.start().ok());

    LMFlowBuffer source{};
    source.data = host.data();
    source.ndim = 1;
    source.dtype = LMFLOW_DTYPE_F32;
    source.shape[0] = kCount;
    source.strides[0] = sizeof(float);
    lmflow::Packet in_packet =
        lmflow::Packet::Adopt(lmflow_packet_from_buffer(&source, /*timestamp=*/0));
    CHECK(!in_packet.IsEmpty());
    CHECK(input.send(std::move(in_packet)).ok());

    std::optional<lmflow::Packet> out = poller.next();
    CHECK(out.has_value());
    LMFlowBuffer result{};
    CHECK(out->AsBuffer(&result));
    CHECK(result.ndim == 1 && result.shape[0] == kCount);
    CHECK(result.dtype == LMFLOW_DTYPE_F32);
    const float* values = static_cast<const float*>(result.data);
    for (int64_t i = 0; i < kCount; ++i) {
      // 2 * 3 == 6:两级 scale 都生效,说明中间那步确实被消费掉了。
      CHECK(values[i] == static_cast<float>(i) * 6.0f);
    }
    graph.close_all_inputs();
  }

  // 2) 中间边的包类型就是 ocl::Image(不是 LMFlowBuffer)—— 「不落主机」的结构性证据。
  {
    lmflow::Graph graph = lmflow::Graph::from_yaml(
        GraphYaml("  - { name: up, kernel: OclUpload, executor: gpu, "
                  "input_ports: [in], output_ports: [out] }\n")
            .c_str());
    CHECK(graph.valid());
    lmflow::Poller poller = graph.add_poller("out");
    lmflow::Input input = graph.input("in");
    CHECK(graph.start().ok());

    LMFlowBuffer source{};
    source.data = host.data();
    source.ndim = 1;
    source.dtype = LMFLOW_DTYPE_F32;
    source.shape[0] = kCount;
    source.strides[0] = sizeof(float);
    CHECK(input.send(lmflow::Packet::Adopt(lmflow_packet_from_buffer(&source, /*timestamp=*/0)))
              .ok());

    std::optional<lmflow::Packet> out = poller.next();
    CHECK(out.has_value());
    CHECK(out->Is<lmflow::ocl::Image>());
    const lmflow::ocl::Image* image = out->TryGet<lmflow::ocl::Image>();
    CHECK(image != nullptr && image->valid());
    CHECK(image->element_count() == static_cast<size_t>(kCount));
    LMFlowBuffer not_a_buffer{};
    CHECK(!out->AsBuffer(&not_a_buffer));  // GPU 句柄从不冒充 LMFlowBuffer
    graph.close_all_inputs();
  }

  // 3) 把 GPU 输出接到只收 LMFLOW_TYPE_BUFFER 的 CPU 输入 —— 必须**建图期**失败。
  //
  // 这是本 adapter 用「自定义类型」而非 LMFlowBuffer.device 字段的全部理由:错误在建图期
  // 就被端口类型校验挡住,而不是留到运行期让 CPU 算子把设备地址当主机地址读。
  {
    bool rejected = false;
    std::string reason;
    try {
      lmflow::Graph graph = lmflow::Graph::from_yaml(
          GraphYaml("  - { name: up, kernel: OclUpload, executor: gpu, "
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
    // 拒绝理由必须点名两侧的类型,否则说明拦下的是别的问题。
    CHECK(reason.find("lmflow.opencl.Image") != std::string::npos);
    CHECK(reason.find("Buffer") != std::string::npos);
  }

  // 4) kernel 缓存:同源同入口重复取,拿到的是同一个 cl_kernel(图重建才不会重编)。
  {
    const std::shared_ptr<lmflow::ocl::Context>& context = lmflow::ocl::Context::Shared();
    cl_kernel first = context->KernelFor(kScaleSource, "scale");
    cl_kernel again = context->KernelFor(kScaleSource, "scale");
    CHECK(first == again);
  }

  std::fprintf(stderr, "lmflow_opencl_test: all checks passed\n");
  return EXIT_SUCCESS;
}
