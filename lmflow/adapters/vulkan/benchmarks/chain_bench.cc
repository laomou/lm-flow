/*
 * chain_bench.cc —— 回答一个具体问题:**把第二个算子也放 GPU、让中间结果留在设备上,
 * 到底值不值?**
 *
 * adapters/vulkan/kernels/resize.cc 写着盈亏平衡在「连续 2~3 个 GPU 算子、中间结果不落主机」
 * 时才出现,但在 affine 落地之前根本没有第二个算子可串,所以那句话一直是推断。这个基准就是
 * 去量它。
 *
 * 与 vulkan_bench.cc 的分工:那个是**分阶段**基准(upload / dispatch / download 各自的成本
 * 来源不同,分开才能把优化归因到阶段)。这个是**图级**基准 —— 因为要比较的两种做法差别不在
 * 某个阶段内部,而在「中间结果过不过 CPU 边界」,只有整条图才能体现,而且这也正是宿主真正
 * 感受到的数字(含引擎开销)。
 *
 * 两条图算出**完全相同的结果**,只差 affine 在哪执行:
 *
 *   resident: VkUpload → VkResize → VkAffine → VkDownload     ← 中间是驻留设备的 vk::Image
 *   split:    VkUpload → VkResize → VkDownload → Affine(CPU)  ← 中间落回主机再算
 *
 * 于是 split - resident 就是「第二个算子留在 GPU」省下的量:一次设备→主机回读、一次主机侧
 * 输出包分配、以及一趟 CPU 逐元素遍历。基准会**校验两条图的输出一致**再报数 —— 算错的基准
 * 没有意义。
 *
 * 形状用真实预处理尺寸(1080p → 模型输入),而不是抽象的等长 buffer:resize → 归一化 是
 * 这一对算子的实际用途,而 resize 会大幅缩小数据量,这恰恰**对 resident 不利**(中间结果
 * 越小,省下的那次回读就越便宜)。用真实形状才不会把结论吹大。
 *
 * ⚠ 与 vulkan_bench.cc 同一条警告:软件实现(lavapipe / llvmpipe)上 dispatch 由 CPU 跑,
 * 设备侧数字**不代表真实 GPU**。主机侧成本(回读、包分配)仍然可信。报告带设备名与
 * software 标记,别拿软件实现的数字去推真机。
 *
 * 首次实测(llvmpipe,7 轮取中位)—— 结论比"串起来就赢"要窄:
 *
 *   1080p → 224x224   中间 0.57 MB   约 -1.5%,但 7 轮里有一轮是 **+1.2%** —— 跨零,
 *                                    也就是**噪声量级,不能声称有收益**
 *   720p  → 360x640   中间 2.64 MB   约 -13.2%(7 轮全为负,区间 -11.4 ~ -15.6%)
 *   1080p → 540x960   中间 5.93 MB   约 -12.9%(7 轮全为负,区间 -10.7 ~ -14.5%)
 *
 * 两点值得记住:
 *
 *   1. **最典型的那个形状收益最小。** 缩到 224x224 模型输入时中间结果只有 0.57 MB,省下的
 *      那次回读本来就便宜,于是被噪声盖住。"resize → 归一化 是最常见的一对"是对的,但恰恰
 *      在这一对上,把 affine 留在 GPU 量不出好处。
 *   2. **收益不随中间结果继续增长** —— 2.64 MB 与 5.93 MB 都是约 -13%。若省的纯是传输字节,
 *      5.93 MB 那档该明显更多。它没有,说明相当一部分省的是**固定成本**:少一个 Download
 *      节点、少一次主机侧输出包分配、少一跳引擎调度。这也意味着算子越多、链越长,收益来自
 *      "少几跳"的部分会累加,而不只是"少搬几个字节"。
 *
 * 所以跑这个基准请**跑多轮看区间**,不要拿单轮的符号下判断;真机数字还需要在真 GPU 上重测
 * (独显的回读比这里贵得多 → 收益更大;移动端统一内存的回读是映射读 → 收益更小)。
 *
 * 输出与 benchmarks/native_throughput.cc 同一 JSON 形状,可直接喂给 compare_reports.py。
 */
#include <vulkan/vulkan.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <iostream>
#include <string>
#include <vector>

#include <lmflow/flow.hpp>
#include <lmflow/vulkan.hpp>

namespace {

using Clock = std::chrono::steady_clock;

/// 一档形状:输入 HxWxC → 输出 out_h x out_w x C。
struct ShapeCase {
  const char* label;
  int64_t in_h;
  int64_t in_w;
  int64_t channels;
  int64_t out_h;
  int64_t out_w;
  std::size_t iterations;
};

struct Result {
  std::string name;
  std::size_t iterations;
  double seconds;
  double bytes_per_iteration;
  double mid_bytes = 0.0;  ///< 中间结果(resize 输出)的字节数 —— 本组对比的驱动变量
};

/// 与 vulkan_bench.cc 一致:报 ms/frame 与 MiB/s。bytes 取**输入**字节数,便于跨档比较。
Result Make(std::string name, std::size_t iterations, double bytes, double seconds,
            double mid_bytes) {
  return Result{std::move(name), iterations, seconds, bytes, mid_bytes};
}

template <typename Body>
double TimeSeconds(std::size_t warmup, std::size_t iterations, Body&& body) {
  for (std::size_t i = 0; i < warmup; ++i) body();
  const auto start = Clock::now();
  for (std::size_t i = 0; i < iterations; ++i) body();
  return std::chrono::duration<double>(Clock::now() - start).count();
}

/// 两条被比较的图。除 affine 落在哪一侧外完全相同 —— 包括 executor 配置,免得把线程池差异
/// 混进结论。
std::string ResidentYaml(int64_t out_h, int64_t out_w, double scale, double shift) {
  char buffer[1024];
  std::snprintf(buffer, sizeof buffer,
                "executors:\n"
                "  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
                "nodes:\n"
                "  - { name: up, kernel: VkUpload, executor: gpu, input_ports: [in], "
                "output_ports: [a] }\n"
                "  - { name: rs, kernel: VkResize, executor: gpu, input_ports: [a], "
                "output_ports: [b], options: { out_h: %lld, out_w: %lld } }\n"
                "  - { name: af, kernel: VkAffine, executor: gpu, input_ports: [b], "
                "output_ports: [c], options: { scale: %g, shift: %g } }\n"
                "  - { name: down, kernel: VkDownload, executor: gpu, input_ports: [c], "
                "output_ports: [out] }\n"
                "input_ports: [in]\n"
                "output_ports: [out]\n",
                static_cast<long long>(out_h), static_cast<long long>(out_w), scale, shift);
  return buffer;
}

std::string SplitYaml(int64_t out_h, int64_t out_w, double scale, double shift) {
  char buffer[1024];
  std::snprintf(buffer, sizeof buffer,
                "executors:\n"
                "  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
                "nodes:\n"
                "  - { name: up, kernel: VkUpload, executor: gpu, input_ports: [in], "
                "output_ports: [a] }\n"
                "  - { name: rs, kernel: VkResize, executor: gpu, input_ports: [a], "
                "output_ports: [b], options: { out_h: %lld, out_w: %lld } }\n"
                "  - { name: down, kernel: VkDownload, executor: gpu, input_ports: [b], "
                "output_ports: [c] }\n"
                "  - { name: af, kernel: AffineKernel, executor: gpu, input_ports: [c], "
                "output_ports: [out], options: { scale: %g, shift: %g } }\n"
                "input_ports: [in]\n"
                "output_ports: [out]\n",
                static_cast<long long>(out_h), static_cast<long long>(out_w), scale, shift);
  return buffer;
}

/// 跑一帧,返回输出的第一个元素与元素总数 —— 用于校验两条图确实算出同一个东西。
struct FrameCheck {
  double first = 0.0;
  double last = 0.0;
  int64_t elements = 0;
};

/// 建图、送一帧、取回一帧。每次调用都重建图 —— 这正是宿主在流水线里反复做的事,也让设备
/// buffer 池化(跨图复用)进入测量范围。
FrameCheck RunOnce(const std::string& yaml, const std::vector<float>& input,
                   const int64_t* in_shape, int in_ndim) {
  lmflow::Graph graph = lmflow::Graph::from_yaml(yaml.c_str());
  lmflow::Poller poller = graph.add_poller("out");
  if (!graph.start().ok()) throw std::runtime_error(graph.last_error());
  lmflow::Input feed = graph.input("in");

  LMFlowBuffer view{};
  view.data = const_cast<float*>(input.data());
  view.ndim = in_ndim;
  view.dtype = LMFLOW_DTYPE_F32;
  int64_t stride = sizeof(float);
  for (int i = in_ndim - 1; i >= 0; --i) {
    view.shape[i] = in_shape[i];
    view.strides[i] = stride;
    stride *= in_shape[i];
  }
  feed.send(lmflow::Packet::Adopt(lmflow_packet_from_buffer(&view, 0)));
  feed.close();

  FrameCheck check;
  if (auto packet = poller.next()) {
    LMFlowBuffer out{};
    if (packet->AsBuffer(&out)) {
      int64_t count = 1;
      for (int i = 0; i < out.ndim; ++i) count *= out.shape[i];
      const float* data = static_cast<const float*>(out.data);
      check.elements = count;
      if (count > 0) {
        check.first = data[0];
        check.last = data[count - 1];
      }
    }
  }
  graph.wait_done();
  return check;
}

}  // namespace

int main(int argc, char** argv) {
  try {
    bool json = false;
    double scale_iters = 1.0;
    for (int i = 1; i < argc; ++i) {
      const std::string argument = argv[i];
      if (argument == "--json") {
        json = true;
      } else {
        scale_iters = std::stod(argument);
      }
    }

    const std::shared_ptr<lmflow::vk::Context>& context = lmflow::vk::Context::Shared();
    VkPhysicalDeviceProperties properties{};
    vkGetPhysicalDeviceProperties(context->physical_device(), &properties);
    const bool software = properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU;

    // 真实预处理形状。224x224 是常见模型输入;另加一档缩小幅度没那么大的,因为 resident
    // 的优势随中间结果变大而变大 —— 只测一档会让结论看起来像是普遍成立。
    const ShapeCase cases[] = {
        {"1080p_to_224", 1080, 1920, 3, 224, 224, 30},
        {"1080p_to_540", 1080, 1920, 3, 540, 960, 20},
        {"720p_to_360", 720, 1280, 3, 360, 640, 30},
    };

    // 归一化:*(1/255) + 0 —— resize → affine 最典型的那一对。
    const double affine_scale = 1.0 / 255.0;
    const double affine_shift = 0.0;

    std::vector<Result> results;
    for (const ShapeCase& shape : cases) {
      const auto iterations = static_cast<std::size_t>(
          std::max(1.0, static_cast<double>(shape.iterations) * scale_iters));
      const std::size_t warmup = iterations > 8 ? 3 : 1;

      const int64_t in_shape[3] = {shape.in_h, shape.in_w, shape.channels};
      const int64_t in_elements = shape.in_h * shape.in_w * shape.channels;
      const double in_bytes = static_cast<double>(in_elements) * sizeof(float);

      std::vector<float> input(static_cast<std::size_t>(in_elements));
      for (std::size_t i = 0; i < input.size(); ++i) {
        input[i] = static_cast<float>(i % 251);  // 251 是质数,避免与通道数产生周期共振
      }

      const std::string resident_yaml =
          ResidentYaml(shape.out_h, shape.out_w, affine_scale, affine_shift);
      const std::string split_yaml =
          SplitYaml(shape.out_h, shape.out_w, affine_scale, affine_shift);

      // 先校验两条图算出同一个东西 —— 否则后面的数字没有可比性。
      const FrameCheck a = RunOnce(resident_yaml, input, in_shape, 3);
      const FrameCheck b = RunOnce(split_yaml, input, in_shape, 3);
      const bool same = a.elements == b.elements && a.elements > 0 &&
                        std::fabs(a.first - b.first) < 1e-4 &&
                        std::fabs(a.last - b.last) < 1e-4;
      if (!same) {
        std::fprintf(stderr,
                     "chain bench: %s 两条图结果不一致(resident %lld 元素 first=%g last=%g / "
                     "split %lld 元素 first=%g last=%g)—— 拒绝报数\n",
                     shape.label, static_cast<long long>(a.elements), a.first, a.last,
                     static_cast<long long>(b.elements), b.first, b.last);
        return EXIT_FAILURE;
      }

      const std::string suffix = std::string("/") + shape.label;
      // 中间结果的大小是这组对比的**驱动变量** —— split 多付的正是它的一次回读 + 一次主机侧
      // 包分配 + 一趟 CPU 遍历。一并报出来,读者不必自己回去算形状。
      const double mid_bytes =
          static_cast<double>(shape.out_h * shape.out_w * shape.channels) * sizeof(float);
      results.push_back(Make("vk/chain_resident" + suffix, iterations, in_bytes,
                             TimeSeconds(warmup, iterations,
                                         [&] { RunOnce(resident_yaml, input, in_shape, 3); }),
                             mid_bytes));
      results.push_back(Make("vk/chain_split" + suffix, iterations, in_bytes,
                             TimeSeconds(warmup, iterations,
                                         [&] { RunOnce(split_yaml, input, in_shape, 3); }),
                             mid_bytes));
    }

    if (json) {
      std::cout << "{\"language\":\"cpp\",\"device\":\"" << properties.deviceName
                << "\",\"software\":" << (software ? "true" : "false") << ",\"results\":[";
      for (std::size_t i = 0; i < results.size(); ++i) {
        if (i != 0) std::cout << ',';
        const Result& r = results[i];
        std::cout << "{\"name\":\"" << r.name << "\",\"iterations\":" << r.iterations
                  << ",\"seconds\":" << r.seconds << ",\"bytes\":" << r.bytes_per_iteration
                  << ",\"mid_bytes\":" << r.mid_bytes << '}';
      }
      std::cout << "]}" << std::endl;
    } else {
      std::printf("device: %s%s\n", properties.deviceName,
                  software ? "  [software —— 设备侧数字不代表真实 GPU]" : "");
      std::printf("%-34s %8s %12s %12s\n", "name", "iters", "ms/frame", "MiB/s");
      for (const Result& r : results) {
        const double ms = r.seconds * 1000.0 / static_cast<double>(r.iterations);
        const double mib = r.bytes_per_iteration * static_cast<double>(r.iterations) /
                           r.seconds / (1024.0 * 1024.0);
        std::printf("%-34s %8zu %12.3f %12.1f\n", r.name.c_str(), r.iterations, ms, mib);
      }
      // 直接把结论算出来,省得读者自己相减 —— 也免得只看绝对值就下判断。
      std::printf("\nresident 相对 split 的差值(负数 = 留在 GPU 更快):\n");
      for (std::size_t i = 0; i + 1 < results.size(); i += 2) {
        const double resident_ms =
            results[i].seconds * 1000.0 / static_cast<double>(results[i].iterations);
        const double split_ms =
            results[i + 1].seconds * 1000.0 / static_cast<double>(results[i + 1].iterations);
        const double delta = (resident_ms - split_ms) / split_ms * 100.0;
        std::printf("  %-24s 中间 %6.2f MB  %+7.1f%%  (%.3f ms vs %.3f ms)\n",
                    results[i].name.substr(results[i].name.find('/') + 1).c_str(),
                    results[i].mid_bytes / (1024.0 * 1024.0), delta, resident_ms, split_ms);
      }
    }
    return EXIT_SUCCESS;
  } catch (const std::exception& error) {
    std::fprintf(stderr, "chain bench failed: %s\n", error.what());
    return EXIT_FAILURE;
  }
}
