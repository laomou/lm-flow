/*
 * chain_bench.cc —— 量一件事:**一条预处理链里的第二个逐元素算子,放 GPU 还是放 CPU?**
 *
 * 起因是 adapters/vulkan/kernels/resize.cc 写着盈亏平衡在「连续 2~3 个 GPU 算子、中间结果
 * 不落主机」时才出现,而在 affine 落地之前没有第二个算子可串,那句话一直是推断。现在两半都
 * 量过了,而且**归因和那句话暗示的相反** —— 见下面的实测表。
 *
 * 与 vulkan_bench.cc 的分工:那个是**分阶段**基准(upload / dispatch / download 各自的成本
 * 来源不同,分开才能把优化归因到阶段)。这个是**图级**基准 —— 因为要比较的两种做法差别不在
 * 某个阶段内部,而在第二个算子跑在哪一侧、以及由此多出的一个图节点,只有整条图才能体现;
 * 而且这也正是宿主真正感受到的数字(含引擎开销)。
 *
 * 两组对比,三条图算出**完全相同的结果**(基准会先校验,不一致就拒绝报数):
 *
 *   mid_resident:      VkUpload → VkResize → VkAffine → VkDownload
 *   mid_roundtrip:     VkUpload → VkResize → VkDownload → VkUpload → VkAffine → VkDownload
 *   second_op_on_cpu:  VkUpload → VkResize → VkDownload → AffineKernel(CPU)
 *
 *   ① 驻留:    resident vs roundtrip —— 两侧 affine **都在 GPU**,只差中间结果多一次
 *               device→host→device 往返。这是 kernels/resize.cc 那句「中间结果不落主机时
 *               才赢」的直接检验,差值里不混"GPU 算术 vs CPU 算术"。
 *   ② 算子落点:resident vs second_op_on_cpu —— 注意这两条图的 Download **搬的字节数相同**
 *               (affine 不改形状),所以这一组**与驻留无关**,纯粹是同一个运算放哪一侧。
 *
 * ── 实测:Adreno 740 / SM8550 / Android 13,8 轮 ────────────────────────────────
 *
 *   中间结果    ① 驻留(中位 / 区间)          ② 算子落点
 *   0.57 MB      -1.3%  (-10.5 ~ +2.6%)  跨零     -2.1%
 *   2.64 MB      -8.9%  (-13.9 ~ +1.5%)  跨零    -17.2%
 *   5.93 MB      -6.8%  (-13.0 ~ +2.8%)  跨零    -17.9%
 *
 * **结论与直觉相反:主项是算子落点,不是驻留。** ② 始终一致为负;① 三档 8 轮**全部跨零**,
 * 这个量级在本机噪声内说不清。原因是统一内存 —— 那次"往返"其实是同一块物理内存里的两次
 * memcpy,不过总线。所以在本项目主打的移动端上,"中间结果不落主机"省下的远没有"别用泛化
 * CPU 循环"多。
 *
 * ② 那一列也是分两段测的:给 cpp/kernels/affine.cc 加 f32/f64 特化快路**之前**是 -9.4% /
 * -33% / -33.6%,加了之后才降到上表的数字 —— 也就是说原始差距有**一半只是那个每元素两次
 * dtype 分派的泛化循环**,与 GPU 无关。
 *
 * ⚠ 这不推翻独显:独显回读要过 PCIe,往返代价应当大得多,但手头没有可枚举的独显,那一档
 * 仍未实测。别把 ① 的结论外推到独显。
 *
 * (三条已修正的记录。早前只有 llvmpipe 数据时,我据"百分比不随中间结果增长"判断收益以固定
 * 成本为主 —— 错的,绝对差值严格线性于元素数。更早我把 ② 那一组称作"驻留是否值得" ——
 * 也是错的,两条图跨边界字节数相同,于是才补了 mid_roundtrip 这条图。三次都是实测暴露的,
 * 不是重读代码发现的。)
 *
 * 跑这个基准请**跑多轮看区间**,不要拿单轮的符号下判断 —— 最小那档在 llvmpipe 上会跨零。
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

/// 第三条图:中间结果**强制往返主机**一次,再交回 GPU 做 affine。
///
/// 这是隔离「驻留」本身的那个对比 —— 与 ResidentYaml 相比,两侧的 affine **都跑在 GPU 上、
/// 用同一个 VkAffine**,唯一差别是中间结果多了一次 device→host→device 往返。于是差值里不再
/// 混入 "GPU 算术 vs CPU 算术",剩下的就是往返本身:一次多余的 Download(含主机侧输出包
/// 分配)+ 一次多余的 Upload。
///
/// 这正是 kernels/resize.cc 那句「中间结果不落主机时才赢」所断言的东西。
std::string RoundtripYaml(int64_t out_h, int64_t out_w, double scale, double shift) {
  char buffer[1400];
  std::snprintf(buffer, sizeof buffer,
                "executors:\n"
                "  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
                "nodes:\n"
                "  - { name: up, kernel: VkUpload, executor: gpu, input_ports: [in], "
                "output_ports: [a] }\n"
                "  - { name: rs, kernel: VkResize, executor: gpu, input_ports: [a], "
                "output_ports: [b], options: { out_h: %lld, out_w: %lld } }\n"
                "  - { name: d1, kernel: VkDownload, executor: gpu, input_ports: [b], "
                "output_ports: [c] }\n"
                "  - { name: u2, kernel: VkUpload, executor: gpu, input_ports: [c], "
                "output_ports: [d] }\n"
                "  - { name: af, kernel: VkAffine, executor: gpu, input_ports: [d], "
                "output_ports: [e], options: { scale: %g, shift: %g } }\n"
                "  - { name: d2, kernel: VkDownload, executor: gpu, input_ports: [e], "
                "output_ports: [out] }\n"
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
      const std::string roundtrip_yaml =
          RoundtripYaml(shape.out_h, shape.out_w, affine_scale, affine_shift);

      // 三条图必须算出同一个东西 —— 否则后面的数字没有可比性。
      const FrameCheck a = RunOnce(resident_yaml, input, in_shape, 3);
      const FrameCheck b = RunOnce(split_yaml, input, in_shape, 3);
      const FrameCheck c = RunOnce(roundtrip_yaml, input, in_shape, 3);
      const auto agrees = [&a](const FrameCheck& x) {
        return x.elements == a.elements && a.elements > 0 &&
               std::fabs(a.first - x.first) < 1e-4 && std::fabs(a.last - x.last) < 1e-4;
      };
      if (!agrees(b) || !agrees(c)) {
        std::fprintf(stderr,
                     "chain bench: %s 三条图结果不一致(gpu %lld/%g/%g  cpu %lld/%g/%g  "
                     "roundtrip %lld/%g/%g)—— 拒绝报数\n",
                     shape.label, static_cast<long long>(a.elements), a.first, a.last,
                     static_cast<long long>(b.elements), b.first, b.last,
                     static_cast<long long>(c.elements), c.first, c.last);
        return EXIT_FAILURE;
      }

      const std::string suffix = std::string("/") + shape.label;
      // 中间结果大小:roundtrip 多付的那一次 Download + Upload 搬的就是它。
      const double mid_bytes =
          static_cast<double>(shape.out_h * shape.out_w * shape.channels) * sizeof(float);
      results.push_back(Make("vk/mid_resident" + suffix, iterations, in_bytes,
                             TimeSeconds(warmup, iterations,
                                         [&] { RunOnce(resident_yaml, input, in_shape, 3); }),
                             mid_bytes));
      results.push_back(Make("vk/mid_roundtrip" + suffix, iterations, in_bytes,
                             TimeSeconds(warmup, iterations,
                                         [&] { RunOnce(roundtrip_yaml, input, in_shape, 3); }),
                             mid_bytes));
      results.push_back(Make("vk/second_op_on_cpu" + suffix, iterations, in_bytes,
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
      // 直接把两组结论算出来,省得读者自己相减。
      // 组一(驻留):两侧 affine 都在 GPU,只差中间结果是否往返主机 —— 这是 resize.cc
      //             「中间结果不落主机时才赢」那句话的直接检验。
      // 组二(算子落点):affine 放 GPU 对放 CPU —— 两条图搬的字节相同,见文件头警告。
      std::printf("\n① 驻留 vs 往返(两侧 affine 都在 GPU,只差中间结果是否落主机):\n");
      for (std::size_t i = 0; i + 2 < results.size() + 1; i += 3) {
        const double res_ms = results[i].seconds * 1000.0 / results[i].iterations;
        const double rt_ms = results[i + 1].seconds * 1000.0 / results[i + 1].iterations;
        std::printf("  %-22s 中间 %6.2f MB  %+7.1f%%  (%.3f ms vs %.3f ms)\n",
                    results[i].name.substr(results[i].name.find('/') + 1).c_str(),
                    results[i].mid_bytes / (1024.0 * 1024.0),
                    (res_ms - rt_ms) / rt_ms * 100.0, res_ms, rt_ms);
      }
      std::printf("\n② affine 放 GPU vs 放 CPU(字节流量相同,不涉及驻留):\n");
      for (std::size_t i = 0; i + 2 < results.size() + 1; i += 3) {
        const double res_ms = results[i].seconds * 1000.0 / results[i].iterations;
        const double cpu_ms = results[i + 2].seconds * 1000.0 / results[i + 2].iterations;
        std::printf("  %-22s 中间 %6.2f MB  %+7.1f%%  (%.3f ms vs %.3f ms)\n",
                    results[i].name.substr(results[i].name.find('/') + 1).c_str(),
                    results[i].mid_bytes / (1024.0 * 1024.0),
                    (res_ms - cpu_ms) / cpu_ms * 100.0, res_ms, cpu_ms);
      }
    }
    return EXIT_SUCCESS;
  } catch (const std::exception& error) {
    std::fprintf(stderr, "chain bench failed: %s\n", error.what());
    return EXIT_FAILURE;
  }
}
