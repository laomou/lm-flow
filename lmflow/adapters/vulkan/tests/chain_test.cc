// chain_test.cc —— VkAffine 数值 + 多算子 GPU 驻留链的测试。
//
// 这是本 adapter 第一个「连续 GPU 段」测试。要点有二:
//
//   1. **affine 数值**:out = in * scale + shift,对照 CPU 参考逐点比对,与 cpp/kernels/affine.cc
//      的 CPU 版同义。单独的 upload→affine→download 用例隔离它自己的正确性。
//   2. **多算子 GPU 驻留链**:VkUpload → VkResize → VkAffine → VkDownload。VkResize 与 VkAffine
//      之间的中间结果是一块 vk::Image,**整条图只有末端一个 VkDownload** —— 中间不落主机。
//      这正是设备 buffer 池化(跨 dispatch 复用)开始回本的场景,也是本 adapter 存在的理由:
//      单个 GPU 逐元素算子净亏,连续两三个才赢。
//
// 三个算子(VkUpload/VkResize/VkAffine + VkDownload)都由 lmflow_vulkan_kernels 档案静态
// 自注册,本测试链的就是该档案,故图里直接引用、无需手写注册。
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

#include <lmflow/flow.hpp>
#include <lmflow/vulkan.hpp>

namespace {

int failures = 0;

void Check(bool ok, const char* what) {
  std::printf("  %s %s\n", ok ? "ok  :" : "FAIL:", what);
  if (!ok) ++failures;
}

/// CPU 参考:双线性,半像素中心(align_corners=false),HWC —— 与 resize_test.cc 同一份约定。
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

/// 填一块非平凡纹理(平坦图测不出插值/逐元素错误)。
std::vector<float> MakeTexture(int h, int w, int channels) {
  std::vector<float> host(static_cast<size_t>(h) * w * channels);
  for (size_t i = 0; i < host.size(); ++i) host[i] = static_cast<float>((i * 37) % 251);
  return host;
}

/// 把 host 数组包成一个 f32 的 LMFlowBuffer(HWC 连续)。strides 交给引擎按 shape 推。
LMFlowBuffer AsBuffer(std::vector<float>& host, int h, int w, int channels) {
  const int ndim = channels == 1 ? 2 : 3;
  LMFlowBuffer b{};
  b.data = host.data();
  b.ndim = ndim;
  b.dtype = LMFLOW_DTYPE_F32;
  b.shape[0] = h;
  b.shape[1] = w;
  b.strides[ndim - 1] = sizeof(float);
  if (ndim == 3) {
    b.shape[2] = channels;
    b.strides[1] = static_cast<int64_t>(channels) * sizeof(float);
    b.strides[0] = static_cast<int64_t>(w) * channels * sizeof(float);
  } else {
    b.strides[0] = static_cast<int64_t>(w) * sizeof(float);
  }
  return b;
}

/// 逐点比对 got 与 want,容差 1e-3(GPU f32 与 CPU f32 的可接受差,含 resize 插值)。
void Compare(const char* label, const float* got, const std::vector<float>& want, bool shape_ok) {
  double worst = 0.0;
  if (shape_ok) {
    for (size_t i = 0; i < want.size(); ++i) {
      worst = std::fmax(worst, std::fabs(static_cast<double>(got[i]) - want[i]));
    }
  }
  Check(shape_ok && worst <= 1e-3, label);
  if (shape_ok && worst > 1e-3) std::printf("       worst abs diff = %g\n", worst);
}

/// 纯 affine:up → af → down。隔离 affine 自身的数值(out = in * scale + shift)。
void RunAffineOnly(const char* label, int h, int w, int channels, double scale, double shift) {
  const int ndim = channels == 1 ? 2 : 3;
  std::vector<float> host = MakeTexture(h, w, channels);
  LMFlowBuffer src = AsBuffer(host, h, w, channels);

  std::string yaml =
      std::string(
          "executors:\n"
          "  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
          "nodes:\n"
          "  - { name: up,   kernel: VkUpload,   executor: gpu, input_ports: [in], "
          "output_ports: [a] }\n"
          "  - { name: af,   kernel: VkAffine,   executor: gpu, input_ports: [a], "
          "output_ports: [b], options: { scale: ") +
      std::to_string(scale) + ", shift: " + std::to_string(shift) +
      " } }\n"
      "  - { name: down, kernel: VkDownload, executor: gpu, input_ports: [b], "
      "output_ports: [out] }\n"
      "input_ports: [in]\noutput_ports: [out]\n";

  try {
    lmflow::Graph graph = lmflow::Graph::from_yaml(yaml.c_str());
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

    std::vector<float> want(host.size());
    for (size_t i = 0; i < host.size(); ++i) {
      want[i] = static_cast<float>(host[i] * scale + shift);
    }
    const bool shape_ok = view.ndim == ndim && view.shape[0] == h && view.shape[1] == w &&
                          (ndim == 2 || view.shape[2] == channels);
    Compare(label, static_cast<const float*>(view.data), want, shape_ok);
  } catch (const std::exception& e) {
    std::printf("  FAIL: %s (%s)\n", label, e.what());
    ++failures;
  }
}

/// 多算子链:up → resize → affine → download。中间 resize→affine 的 vk::Image 不落主机。
/// oracle = affine(resize(in)):先 CPU 参考 resize,再逐元素 *scale + shift。
void RunChain(const char* label, int in_h, int in_w, int channels, int out_h, int out_w,
              double scale, double shift) {
  const int ndim = channels == 1 ? 2 : 3;
  std::vector<float> host = MakeTexture(in_h, in_w, channels);
  LMFlowBuffer src = AsBuffer(host, in_h, in_w, channels);

  std::string yaml =
      std::string(
          "executors:\n"
          "  - { name: gpu, type: ThreadPoolExecutor, num_threads: 1 }\n"
          "nodes:\n"
          "  - { name: up,   kernel: VkUpload,   executor: gpu, input_ports: [in], "
          "output_ports: [a] }\n"
          "  - { name: rs,   kernel: VkResize,   executor: gpu, input_ports: [a], "
          "output_ports: [b], options: { out_h: ") +
      std::to_string(out_h) + ", out_w: " + std::to_string(out_w) +
      " } }\n"
      "  - { name: af,   kernel: VkAffine,   executor: gpu, input_ports: [b], "
      "output_ports: [c], options: { scale: " +
      std::to_string(scale) + ", shift: " + std::to_string(shift) +
      " } }\n"
      "  - { name: down, kernel: VkDownload, executor: gpu, input_ports: [c], "
      "output_ports: [out] }\n"
      "input_ports: [in]\noutput_ports: [out]\n";

  try {
    lmflow::Graph graph = lmflow::Graph::from_yaml(yaml.c_str());
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

    std::vector<float> want = ReferenceResize(host, in_h, in_w, channels, out_h, out_w);
    for (float& v : want) v = static_cast<float>(v * scale + shift);
    const bool shape_ok = view.ndim == ndim && view.shape[0] == out_h && view.shape[1] == out_w &&
                          (ndim == 2 || view.shape[2] == channels);
    Compare(label, static_cast<const float*>(view.data), want, shape_ok);
  } catch (const std::exception& e) {
    std::printf("  FAIL: %s (%s)\n", label, e.what());
    ++failures;
  }
}

}  // namespace

int main() {
  try {
    lmflow::vk::Context::Shared();
  } catch (const std::exception& e) {
    std::printf("skipping: no usable Vulkan device (%s)\n", e.what());
    return 0;
  }

  std::printf("VkAffine 数值(out = in * scale + shift,对照 CPU 参考):\n");
  RunAffineOnly("2D 8x8  scale=2 shift=1", 8, 8, 1, 2.0, 1.0);
  RunAffineOnly("3D 6x5x3 scale=0.5 shift=-3", 6, 5, 3, 0.5, -3.0);
  RunAffineOnly("恒等 scale=1 shift=0", 7, 4, 1, 1.0, 0.0);

  std::printf("多算子 GPU 驻留链(VkUpload→VkResize→VkAffine→VkDownload,中间不落主机):\n");
  RunChain("缩小 2D 16x16 -> 5x5, *2+10", 16, 16, 1, 5, 5, 2.0, 10.0);
  RunChain("放大 3D 4x4x3 -> 9x7x3, *0.25-1", 4, 4, 3, 9, 7, 0.25, -1.0);
  RunChain("非等比 3D 12x20x3 -> 7x5x3, 归一化 /255", 12, 20, 3, 7, 5, 1.0 / 255.0, 0.0);

  if (failures == 0) {
    std::printf("全部通过\n");
    return EXIT_SUCCESS;
  }
  std::printf("%d 个用例失败\n", failures);
  return EXIT_FAILURE;
}
