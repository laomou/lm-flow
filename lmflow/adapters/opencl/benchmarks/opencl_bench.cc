/*
 * opencl_bench.cc —— OpenCL adapter 的分阶段基准,与 adapters/vulkan/benchmarks 同构。
 *
 * 分阶段而不是只测整条链的理由同 Vulkan 版:upload / dispatch / download 的成本来源
 * 各不相同(阻塞式主机拷贝 vs 设参与入队 vs 回读 + 输出包分配),混在一起就无法把
 * 某次改动归因到具体阶段。
 *
 * 输出与 benchmarks/native_throughput.cc 同一 JSON 形状,可直接喂给
 * benchmarks/compare_reports.py:
 *   lmflow_opencl_bench --json > before.json  →  改动  →  after.json  →  compare
 *
 * 与 Vulkan 版的一个实质差别:本 adapter 的 Upload/Download 都是**阻塞式**
 * (clEnqueueWriteBuffer / clEnqueueReadBuffer 传 CL_TRUE),而 EnqueueUnary 是异步的,
 * 故 dispatch_enqueue 测到的是 CPU 侧的设参 + 入队成本,不含 GPU 执行时间。
 *
 * ⚠ 本二进制由 CMake 单独打开了 LMFLOW_OCL_PROFILING(dispatch_gpu 那一行需要它)。
 * 代价实测:每次 dispatch 多约 5µs,于是这里的 dispatch_enqueue 比**默认构建**偏高
 * (小 buffer 上 +56%、整条回路 +6%;大 buffer 因传输占主导只 1~3%)。要拿默认构建的
 * 主机侧数字,请把 LMFLOW_OCL_PROFILING 关掉重编。
 */
#ifndef CL_TARGET_OPENCL_VERSION
#define CL_TARGET_OPENCL_VERSION 120
#endif

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <iostream>
#include <string>
#include <vector>

#include <lmflow/opencl.hpp>

namespace {

using Clock = std::chrono::steady_clock;

const char* kScaleSource = R"CLC(
__kernel void scale(__global const float* in, __global float* out, const float factor) {
  const size_t i = get_global_id(0);
  out[i] = in[i] * factor;
}
)CLC";

struct Result {
  std::string name;
  std::size_t iterations;
  std::size_t payload_bytes;
  double packets_per_second;
  double nanoseconds_per_packet;
};

Result Make(const std::string& name, std::size_t iterations, std::size_t payload_bytes,
            double seconds) {
  Result result;
  result.name = name;
  result.iterations = iterations;
  result.payload_bytes = payload_bytes;
  result.packets_per_second = seconds > 0.0 ? static_cast<double>(iterations) / seconds : 0.0;
  result.nanoseconds_per_packet =
      iterations != 0 ? seconds * 1e9 / static_cast<double>(iterations) : 0.0;
  return result;
}

template <typename Body>
double TimeSeconds(std::size_t warmup, std::size_t iterations, Body&& body) {
  for (std::size_t i = 0; i < warmup; ++i) body();
  const auto start = Clock::now();
  for (std::size_t i = 0; i < iterations; ++i) body();
  return std::chrono::duration<double>(Clock::now() - start).count();
}

/// 可复用的主机侧 f32 源缓冲(用引擎分配,贴近真实算子的输入形态)。
class HostSource {
 public:
  explicit HostSource(std::size_t elements) : elements_(elements) {
    const int64_t shape[1] = {static_cast<int64_t>(elements)};
    packet_ = lmflow::Packet::Adopt(
        lmflow_packet_new_buffer(1, shape, LMFLOW_DTYPE_F32, LMFLOW_TS_UNSET, &view_));
    if (packet_.IsEmpty()) throw std::runtime_error("bench: lmflow_packet_new_buffer failed");
    float* data = static_cast<float*>(view_.data);
    for (std::size_t i = 0; i < elements; ++i) data[i] = static_cast<float>(i % 251);
  }

  const LMFlowBuffer& view() const { return view_; }
  std::size_t bytes() const { return elements_ * sizeof(float); }

 private:
  std::size_t elements_;
  LMFlowBuffer view_{};
  lmflow::Packet packet_;
};

struct SizeCase {
  const char* label;
  std::size_t elements;
  std::size_t iterations;
};

}  // namespace

int main(int argc, char** argv) {
  try {
    bool json = false;
    double scale = 1.0;
    for (int i = 1; i < argc; ++i) {
      const std::string argument = argv[i];
      if (argument == "--json") {
        json = true;
      } else {
        scale = std::stod(argument);
      }
    }

    const std::shared_ptr<lmflow::ocl::Context>& context = lmflow::ocl::Context::Shared();

    char device_name[256] = {0};
    cl_device_type device_type = 0;
    cl_bool host_unified = CL_FALSE;
    clGetDeviceInfo(context->device(), CL_DEVICE_NAME, sizeof device_name, device_name, nullptr);
    clGetDeviceInfo(context->device(), CL_DEVICE_TYPE, sizeof device_type, &device_type, nullptr);
    clGetDeviceInfo(context->device(), CL_DEVICE_HOST_UNIFIED_MEMORY, sizeof host_unified,
                    &host_unified, nullptr);
    const bool software = (device_type & CL_DEVICE_TYPE_GPU) == 0;

    const SizeCase cases[] = {
        {"1mb", 262144u, 200u},
        {"4mb", 1048576u, 60u},
        {"24mb", 1920u * 1080u * 3u, 12u},
    };

    std::vector<Result> results;
    for (const SizeCase& size : cases) {
      const auto iterations =
          static_cast<std::size_t>(std::max(1.0, static_cast<double>(size.iterations) * scale));
      const std::size_t warmup = iterations > 8 ? 3 : 1;
      HostSource source(size.elements);
      const std::string suffix = std::string("/") + size.label;
      const float factor = 2.0f;
      auto set_factor = [factor](cl_kernel kernel) {
        lmflow::ocl::Check(clSetKernelArg(kernel, 2, sizeof factor, &factor), "clSetKernelArg(2)");
      };

      // upload:阻塞式 clEnqueueWriteBuffer,含每帧一次 clCreateBuffer
      results.push_back(Make("ocl/upload" + suffix, iterations, source.bytes(),
                             TimeSeconds(warmup, iterations, [&] {
                               lmflow::ocl::Image image =
                                   lmflow::ocl::Upload(context, source.view());
                               (void)image;
                             })));

      // dispatch_enqueue:只测 CPU 侧成本(kernel 缓存查找 + 设参 + 入队),不等 GPU
      {
        lmflow::ocl::Image input = lmflow::ocl::Upload(context, source.view());
        results.push_back(Make("ocl/dispatch_enqueue" + suffix, iterations, source.bytes(),
                               TimeSeconds(warmup, iterations, [&] {
                                 lmflow::ocl::Image output = lmflow::ocl::EnqueueUnary(
                                     input, kScaleSource, "scale", set_factor);
                                 (void)output;
                               })));
      }

      // dispatch_gpu:同一次 dispatch 的 **GPU 执行**耗时(靠 CL_QUEUE_PROFILING_ENABLE)。
      // 与 dispatch_enqueue 对照才能把「CPU 入队」和「GPU 执行」分开 —— 只看 enqueue
      // 会误以为那就是纯 CPU 成本,而它其实会被队列背压拖长。
      {
        lmflow::ocl::Image input = lmflow::ocl::Upload(context, source.view());
        std::vector<double> gpu_ns;
        gpu_ns.reserve(iterations);
        for (std::size_t i = 0; i < iterations; ++i) {
          lmflow::ocl::Image out =
              lmflow::ocl::EnqueueUnary(input, kScaleSource, "scale", set_factor);
          cl_event event = out.ready();
          if (event) clWaitForEvents(1, &event);
          gpu_ns.push_back(static_cast<double>(out.gpu_duration_ns()));
        }
        std::sort(gpu_ns.begin(), gpu_ns.end());
        Result gpu;
        gpu.name = "ocl/dispatch_gpu" + suffix;
        gpu.iterations = iterations;
        gpu.payload_bytes = source.bytes();
        gpu.nanoseconds_per_packet = gpu_ns[gpu_ns.size() / 2];
        gpu.packets_per_second =
            gpu.nanoseconds_per_packet > 0 ? 1e9 / gpu.nanoseconds_per_packet : 0.0;
        results.push_back(gpu);
      }

      // download:阻塞式回读。含输出包分配 + 等生产者 event + 拷回主机
      {
        lmflow::ocl::Image image = lmflow::ocl::Upload(context, source.view());
        results.push_back(Make("ocl/download" + suffix, iterations, source.bytes(),
                               TimeSeconds(warmup, iterations, [&] {
                                 lmflow::Packet packet = lmflow::ocl::Download(image);
                                 (void)packet;
                               })));
      }

      // roundtrip:upload → scale → download,一次真实的完整回路
      results.push_back(Make(
          "ocl/roundtrip" + suffix, iterations, source.bytes(),
          TimeSeconds(warmup, iterations, [&] {
            lmflow::ocl::Image uploaded = lmflow::ocl::Upload(context, source.view());
            lmflow::ocl::Image scaled =
                lmflow::ocl::EnqueueUnary(uploaded, kScaleSource, "scale", set_factor);
            lmflow::Packet packet = lmflow::ocl::Download(scaled);
            (void)packet;
          })));
    }

    if (json) {
      std::cout << "{\"language\":\"cpp\",\"device\":\"" << device_name
                << "\",\"software\":" << (software ? "true" : "false")
                << ",\"host_unified_memory\":" << (host_unified ? "true" : "false")
                << ",\"results\":[";
      for (std::size_t i = 0; i < results.size(); ++i) {
        if (i != 0) std::cout << ',';
        const Result& r = results[i];
        std::cout << "{\"name\":\"" << r.name << "\",\"iterations\":" << r.iterations
                  << ",\"payload_bytes\":" << r.payload_bytes
                  << ",\"packets_per_second\":" << r.packets_per_second
                  << ",\"nanoseconds_per_packet\":" << r.nanoseconds_per_packet;
        if (r.payload_bytes != 0) {
          std::cout << ",\"mib_per_second\":"
                    << r.packets_per_second * static_cast<double>(r.payload_bytes) /
                           (1024.0 * 1024.0);
        }
        std::cout << '}';
      }
      std::cout << "]}\n";
    } else {
      std::printf("device: %s%s  host_unified=%s\n", device_name,
                  software ? "  [非 GPU —— 设备侧数字不代表真机]" : "",
                  host_unified ? "yes" : "no");
      std::printf("%-28s %10s %14s %14s\n", "name", "iters", "ms/frame", "MiB/s");
      for (const Result& r : results) {
        std::printf("%-28s %10zu %14.3f %14.1f\n", r.name.c_str(), r.iterations,
                    r.nanoseconds_per_packet / 1e6,
                    r.packets_per_second * static_cast<double>(r.payload_bytes) /
                        (1024.0 * 1024.0));
      }
    }
    return 0;
  } catch (const std::exception& error) {
    std::fprintf(stderr, "opencl bench failed: %s\n", error.what());
    return 1;
  }
}
