/*
 * vulkan_bench.cc —— Vulkan adapter 的分阶段基准。
 *
 * 为什么分阶段而不是只测整条链:upload / dispatch / download 的成本来源完全不同
 * (主机拷贝 vs 命令录制与提交 vs 回读 + 输出包分配),混在一起就无法把某次优化
 * 归因到具体阶段。各阶段单独可测,才能回答「这次改动到底省在哪」。
 *
 * 输出与 benchmarks/native_throughput.cc 同一 JSON 形状,故可直接喂给
 * benchmarks/compare_reports.py 做前后对比:
 *   lmflow_vulkan_bench --json > before.json
 *   ...改动...
 *   lmflow_vulkan_bench --json > after.json
 *   python3 lmflow/benchmarks/compare_reports.py before.json after.json
 *
 * ⚠ 设备类型决定数字有没有意义:软件实现(llvmpipe 等)上 vkAllocateMemory 只是一次
 * malloc、dispatch 由 CPU 跑,**设备侧数字不代表真实 GPU**;主机侧成本(回读拷贝、
 * 输出包分配)仍然是可信的。故报告里带上设备名与 software 标记,别拿软件实现的
 * dispatch 数字去推真机。
 */
#include <vulkan/vulkan.h>

#include <chrono>
#include <cstdint>
#include <cstdio>
#include <iostream>
#include <string>
#include <vector>

#include <lmflow/vulkan.hpp>

#include "../tests/scale_spv.h"

namespace {

using Clock = std::chrono::steady_clock;

struct ScaleParams {
  float factor;
  uint32_t count;
};

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

/// 计时:先热身,再按 total/iterations 取吞吐(与 native_throughput 的语义一致)。
template <typename Body>
double TimeSeconds(std::size_t warmup, std::size_t iterations, Body&& body) {
  for (std::size_t i = 0; i < warmup; ++i) body();
  const auto start = Clock::now();
  for (std::size_t i = 0; i < iterations; ++i) body();
  return std::chrono::duration<double>(Clock::now() - start).count();
}

/// 一块可复用的主机侧 f32 源缓冲(用引擎分配,贴近真实算子的输入形态)。
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
        scale = std::stod(argument);  // 迭代次数倍率,便于在慢设备上压低耗时
      }
    }

    const std::shared_ptr<lmflow::vk::Context>& context = lmflow::vk::Context::Shared();

    // 设备信息:决定下面的设备侧数字能不能当真
    VkPhysicalDeviceProperties properties{};
    vkGetPhysicalDeviceProperties(context->physical_device(), &properties);
    const bool software = properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU;

    // 1 MB / 4 MB / 24 MB(1080p RGB f32)三档,迭代数随体积递减
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

      // upload:主机 → 设备(统一内存直接映射;独显走 staging + 队列拷贝)
      results.push_back(Make(
          "vk/upload" + suffix, iterations, source.bytes(),
          TimeSeconds(warmup, iterations, [&] {
            lmflow::vk::Image image = lmflow::vk::Upload(context, source.view());
            (void)image;
          })));

      // dispatch:只测 CPU 侧的录制 + 提交成本(不等 GPU)—— shader cache 查找、
      // 命令缓冲/descriptor set 分配、vkQueueSubmit 都落在这里
      {
        lmflow::vk::Image input = lmflow::vk::Upload(context, source.view());
        ScaleParams params{2.0f, static_cast<uint32_t>(size.elements)};
        results.push_back(Make(
            "vk/dispatch_enqueue" + suffix, iterations, source.bytes(),
            TimeSeconds(warmup, iterations, [&] {
              lmflow::vk::Image output =
                  lmflow::vk::EnqueueUnary(input, kScaleSpv, sizeof kScaleSpv / sizeof kScaleSpv[0],
                                           "main", &params, sizeof params);
              (void)output;
            })));
      }

      // download:设备 → 主机。含输出包分配(目前会被清零)+ 等时间线 + 回读拷贝
      {
        lmflow::vk::Image image = lmflow::vk::Upload(context, source.view());
        results.push_back(Make("vk/download" + suffix, iterations, source.bytes(),
                               TimeSeconds(warmup, iterations, [&] {
                                 lmflow::Packet packet = lmflow::vk::Download(image);
                                 (void)packet;
                               })));
      }

      // roundtrip:upload → scale → download,一次真实的 GPU→CPU 完整回路
      {
        ScaleParams params{2.0f, static_cast<uint32_t>(size.elements)};
        results.push_back(Make(
            "vk/roundtrip" + suffix, iterations, source.bytes(),
            TimeSeconds(warmup, iterations, [&] {
              lmflow::vk::Image uploaded = lmflow::vk::Upload(context, source.view());
              lmflow::vk::Image scaled =
                  lmflow::vk::EnqueueUnary(uploaded, kScaleSpv,
                                           sizeof kScaleSpv / sizeof kScaleSpv[0], "main", &params,
                                           sizeof params);
              lmflow::Packet packet = lmflow::vk::Download(scaled);
              (void)packet;
            })));
      }

      // 拷贝式 vs 零拷贝下载的对比。两者都含一次 Upload(DownloadMapped 会消耗掉持有
      // Image 的包,没法像上面那样复用同一个 Image),故差值即零拷贝省下的部分。
      {
        lmflow::vk::Image probe = lmflow::vk::Upload(context, source.view());
        const bool mapped_ok = lmflow::vk::CanDownloadMapped(probe);
        results.push_back(Make("vk/upload_download" + suffix, iterations, source.bytes(),
                               TimeSeconds(warmup, iterations, [&] {
                                 lmflow::vk::Image image =
                                     lmflow::vk::Upload(context, source.view());
                                 lmflow::Packet packet = lmflow::vk::Download(image);
                                 (void)packet;
                               })));
        if (mapped_ok) {
          results.push_back(Make(
              "vk/upload_download_mapped" + suffix, iterations, source.bytes(),
              TimeSeconds(warmup, iterations, [&] {
                lmflow::Packet held = lmflow::Packet::Make<lmflow::vk::Image>(
                    lmflow::vk::Upload(context, source.view()));
                lmflow::Packet packet = lmflow::vk::DownloadMapped(std::move(held));
                (void)packet;
              })));
        }
      }
    }

    if (json) {
      std::cout << "{\"language\":\"cpp\",\"device\":\"" << properties.deviceName
                << "\",\"software\":" << (software ? "true" : "false") << ",\"results\":[";
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
      std::printf("device: %s%s\n", properties.deviceName,
                  software ? "  [软件实现 —— 设备侧数字不代表真机]" : "");
      std::printf("%-28s %10s %14s %14s\n", "name", "iters", "ms/frame", "MiB/s");
      for (const Result& r : results) {
        std::printf("%-28s %10zu %14.3f %14.1f\n", r.name.c_str(), r.iterations,
                    r.nanoseconds_per_packet / 1e6,
                    r.packets_per_second * static_cast<double>(r.payload_bytes) / (1024.0 * 1024.0));
      }
    }
    return 0;
  } catch (const std::exception& error) {
    std::fprintf(stderr, "vulkan bench failed: %s\n", error.what());
    return 1;
  }
}
