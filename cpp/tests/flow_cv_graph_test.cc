// 证明「只要 flow_cv.hpp 这个头 + 导出的 C ABI,用户就能写一个 OpenCV C++ 算子、
// 注册进图、端到端跑起来」—— 框架本身一行 OpenCV 都不编、不需要任何开关(ADR #14)。
//
// 与 flow_cv_test.cc 的区别:那个只测转换原语(Mat⇄LmflowBuffer);这个把 OpenCV 算子
// **放进真图**跑一遍(输入图像 → 算子 → 读回),喂的输入就是 Python send(numpy) 会产生
// 的同一种 LmflowBuffer,故也缝合了「Python 图像 → C++ OpenCV 算子」那条数据路。
//
// 编译(命令是一条,续行不写进注释以免 -Werror=comment):
//   见 .github/workflows/ci.yml 的 opencv job。

#include <cassert>
#include <cstdint>
#include <cstdio>

#include "flow.h"
#include "flow.hpp"
#include "flow_cv.hpp"

// 用户自己的 OpenCV 算子:把输入图像取反(255 - x)。只依赖 flow_cv.hpp 与 OpenCV。
class CvInvertKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetAny(0);
    c.OutputSetAny(0);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    lmflow::Packet p = cc.TakeInput(0);  // 取走 → 独占,CoW 零拷贝
    cv::Mat m;
    if (LmflowStatus st = lmflow::CvMutable(p, &m)) return st;  // LmflowBuffer → 可写 cv::Mat
    cv::bitwise_not(m, m);                                  // OpenCV 就地取反
    cc.Emit(0, std::move(p));
    return lmflow::Status::Ok();
  }
};

int main() {
  lmflow_register_kernel("CvInvert", lmflow::KernelAdapter<CvInvertKernel>::vtable(), nullptr);

  LmflowGraph* g = lmflow_graph_new();
  const char* yaml =
      "nodes:\n"
      "  - { name: inv, kernel: CvInvert, input_ports: [in], output_ports: [out] }\n"
      "input_ports: [in]\n"
      "output_ports: [out]\n";
  assert(lmflow_graph_init_from_yaml(g, yaml) == 0);
  LmflowPoller* poller = lmflow_graph_add_poller(g, "out");
  assert(poller);
  assert(lmflow_graph_start(g) == 0);
  LmflowInput* in = lmflow_graph_input(g, "in");
  assert(in);

  // 输入:一张 2x3 U8 图(模拟 Python 送进来的 numpy/cv2 图 → 同一种 LmflowBuffer)
  cv::Mat img(2, 3, CV_8UC1);
  for (int i = 0; i < 6; ++i) img.data[i] = static_cast<uint8_t>(i * 10);  // 0,10,20,30,40,50
  LmflowPacket pkt = lmflow::PacketFromMat(img).At(0).release();               // 拷进引擎
  assert(lmflow_input_send(in, pkt) == 0);

  LmflowPacket out{};
  assert(lmflow_poller_next(poller, &out) && "算子应产出一张图");
  cv::Mat result = lmflow::CvView(lmflow::Packet::Borrow(out));  // 只读视图读回
  assert(result.rows == 2 && result.cols == 3 && result.channels() == 1);
  for (int i = 0; i < 6; ++i) {
    assert(result.data[i] == static_cast<uint8_t>(255 - i * 10) && "应逐像素取反");
  }
  lmflow_packet_drop(&out);

  lmflow_graph_close_all_inputs(g);
  assert(lmflow_graph_wait_done(g) == 0);
  lmflow_input_free(in);
  lmflow_poller_free(poller);
  lmflow_graph_free(g);

  std::printf("OK:OpenCV C++ 算子在真图里端到端跑通(只用 flow_cv.hpp 头 + C ABI)\n");
  return 0;
}
