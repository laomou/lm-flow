// VkResize 的数值与契约测试。
//
// 关键是**对照一份 CPU 参考实现**,而不是只看跑通:resize 最容易出的偏差是插值约定不一致
// (align_corners、半像素中心),那种错误结果看起来完全"正常",只是像素整体偏半个格。
// 参考实现与 kernel 用同一套约定 `src = (dst + 0.5) * scale - 0.5`,逐点比对。
#include <cmath>
#include <cstdio>
#include <string>
#include <vector>

#include <lmflow/vulkan.hpp>

namespace {

using VkNothing = void;  // 占位,避免未使用告警的干扰

int failures = 0;

void Check(bool ok, const char* what) {
  std::printf("  %s %s\n", ok ? "ok  :" : "FAIL:", what);
  if (!ok) ++failures;
}

/// CPU 参考:双线性,半像素中心(align_corners=false),HWC。
std::vector<float> ReferenceResize(const std::vector<float>& src, int in_h, int in_w, int channels,
                                   int out_h, int out_w) {
  std::vector<float> dst(static_cast<size_t>(out_h) * out_w * channels);
  const float scale_y = static_cast<float>(in_h) / static_cast<float>(out_h);
  const float scale_x = static_cast<float>(in_w) / static_cast<float>(out_w);
  for (int y = 0; y < out_h; ++y) {
    for (int x = 0; x < out_w; ++x) {
      float sy = (static_cast<float>(y) + 0.5f) * scale_y - 0.5f;
      float sx = (static_cast<float>(x) + 0.5f) * scale_x - 0.5f;
      sy = sy < 0.0f ? 0.0f : sy;
      sx = sx < 0.0f ? 0.0f : sx;
      int y0 = static_cast<int>(sy);
      int x0 = static_cast<int>(sx);
      if (y0 > in_h - 1) y0 = in_h - 1;
      if (x0 > in_w - 1) x0 = in_w - 1;
      const int y1 = y0 + 1 < in_h ? y0 + 1 : in_h - 1;
      const int x1 = x0 + 1 < in_w ? x0 + 1 : in_w - 1;
      const float wy = sy - static_cast<float>(y0);
      const float wx = sx - static_cast<float>(x0);
      for (int c = 0; c < channels; ++c) {
        const float v00 = src[(static_cast<size_t>(y0) * in_w + x0) * channels + c];
        const float v01 = src[(static_cast<size_t>(y0) * in_w + x1) * channels + c];
        const float v10 = src[(static_cast<size_t>(y1) * in_w + x0) * channels + c];
        const float v11 = src[(static_cast<size_t>(y1) * in_w + x1) * channels + c];
        const float top = v00 + (v01 - v00) * wx;
        const float bottom = v10 + (v11 - v10) * wx;
        dst[(static_cast<size_t>(y) * out_w + x) * channels + c] = top + (bottom - top) * wy;
      }
    }
  }
  return dst;
}

std::string GraphYaml(int out_h, int out_w) {
  return std::string(
             "executors:\n"
             "  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
             "nodes:\n"
             "  - { name: up, kernel: VkUpload, executor: gpu, "
             "input_ports: [in], output_ports: [a] }\n"
             "  - { name: rs, kernel: VkResize, executor: gpu, "
             "input_ports: [a], output_ports: [b], options: { out_h: ") +
         std::to_string(out_h) + ", out_w: " + std::to_string(out_w) +
         " } }\n"
         "  - { name: down, kernel: VkDownload, executor: gpu, "
         "input_ports: [b], output_ports: [out] }\n"
         "input_ports: [in]\noutput_ports: [out]\n";
}

/// 跑一次 upload→resize→download,并与 CPU 参考逐点比对。
void RunCase(const char* label, int in_h, int in_w, int channels, int out_h, int out_w) {
  const int ndim = channels == 1 ? 2 : 3;
  std::vector<float> host(static_cast<size_t>(in_h) * in_w * channels);
  for (size_t i = 0; i < host.size(); ++i) {
    host[i] = static_cast<float>((i * 37) % 251);  // 非平凡纹理,平坦图测不出插值错误
  }

  LMFlowBuffer src{};
  src.data = host.data();
  src.ndim = ndim;
  src.dtype = LMFLOW_DTYPE_F32;
  src.shape[0] = in_h;
  src.shape[1] = in_w;
  src.strides[ndim - 1] = sizeof(float);
  if (ndim == 3) {
    src.shape[2] = channels;
    src.strides[1] = static_cast<int64_t>(channels) * sizeof(float);
    src.strides[0] = static_cast<int64_t>(in_w) * channels * sizeof(float);
  } else {
    src.strides[0] = static_cast<int64_t>(in_w) * sizeof(float);
  }

  try {
    lmflow::Graph graph = lmflow::Graph::from_yaml(GraphYaml(out_h, out_w).c_str());
    lmflow::Poller poller = graph.add_poller("out");
    lmflow::Input input = graph.input("in");
    if (!graph.start().ok()) { Check(false, label); return; }
    if (!input.send(lmflow::Packet::Adopt(lmflow_packet_from_buffer(&src, 0))).ok()) {
      Check(false, label);
      return;
    }
    input.close();
    auto out = poller.next();
    if (!out) { Check(false, label); return; }
    LMFlowBuffer view{};
    if (!out->AsBuffer(&view)) { Check(false, label); return; }

    const std::vector<float> want = ReferenceResize(host, in_h, in_w, channels, out_h, out_w);
    const float* got = static_cast<const float*>(view.data);
    bool shape_ok = view.ndim == ndim && view.shape[0] == out_h && view.shape[1] == out_w &&
                    (ndim == 2 || view.shape[2] == channels);
    double worst = 0.0;
    size_t worst_at = 0;
    if (shape_ok) {
      for (size_t i = 0; i < want.size(); ++i) {
        const double d = std::fabs(static_cast<double>(got[i]) - want[i]);
        if (d > worst) { worst = d; worst_at = i; }
      }
    }
    // 容差:GPU 与 CPU 的浮点运算顺序可能不同,但双线性只有几次乘加,偏差应远小于 1e-3。
    const bool ok = shape_ok && worst < 1e-3;
    std::printf("  %s %s (最大偏差 %.3g%s)\n", ok ? "ok  :" : "FAIL:", label, worst,
                shape_ok ? "" : ", 形状不符");
    if (!ok) {
      if (shape_ok) {
        std::printf("        worst idx=%zu got=%f want=%f\n", worst_at, got[worst_at],
                    want[worst_at]);
      }
      ++failures;
    }
    graph.wait_done();
  } catch (const std::exception& e) {
    std::printf("  FAIL: %s 抛异常 %s\n", label, e.what());
    ++failures;
  }
}

}  // namespace

// VkResize 由 lmflow_opencl_kernels 档案静态自注册,**此处不能再注册**。
// upload/download 是 adapter 头里的类,按约定由宿主注册。
using VkUploadKernel = lmflow::vk::UploadKernel;
using VkDownloadKernel = lmflow::vk::DownloadKernel;
LMFLOW_REGISTER_KERNEL_AS(VkUploadKernel, "VkUpload")
LMFLOW_REGISTER_KERNEL_AS(VkDownloadKernel, "VkDownload")

int main() {
  try {
    lmflow::vk::Context::Shared();
  } catch (const std::exception& e) {
    std::printf("skipping: no usable Vulkan device (%s)\n", e.what());
    return 0;
  }

  std::printf("VkResize 数值对照 CPU 参考(半像素中心双线性):\n");
  RunCase("缩小 2D 8x8 -> 4x4", 8, 8, 1, 4, 4);
  RunCase("放大 2D 4x4 -> 9x7", 4, 4, 1, 9, 7);
  RunCase("缩小 3D 16x16x3 -> 5x5x3", 16, 16, 3, 5, 5);
  RunCase("非等比 3D 12x20x3 -> 7x5x3", 12, 20, 3, 7, 5);
  RunCase("同尺寸应为恒等 2D 6x6 -> 6x6", 6, 6, 1, 6, 6);

  std::printf("契约:\n");
  // 缺 option 必须在 Open 期失败,而不是静默用默认值
  {
    const char* yaml =
        "executors:\n  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
        "nodes:\n  - { name: rs, kernel: VkResize, executor: gpu, "
        "input_ports: [in], output_ports: [out] }\n"
        "input_ports: [in]\noutput_ports: [out]\n";
    bool rejected = false;
    try {
      lmflow::Graph graph = lmflow::Graph::from_yaml(yaml);
      rejected = !graph.start().ok();
    } catch (const std::exception&) {
      rejected = true;
    }
    Check(rejected, "缺 out_h/out_w 时在 start 期失败");
  }

  std::printf("%s\n", failures == 0 ? "lmflow_vulkan_resize_test: all checks passed"
                                    : "lmflow_vulkan_resize_test: FAILED");
  return failures == 0 ? 0 : 1;
}
