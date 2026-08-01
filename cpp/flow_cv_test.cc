// flow_cv.hpp 的单元测试 —— 需要 OpenCV,并链接引擎库(用到 flow_packet_* C ABI)。
//
//   g++ -std=c++17 -Iinclude cpp/flow_cv_test.cc target/release/libflow_core.a
//       $(pkg-config --cflags --libs opencv4) -lpthread -ldl -lm -o flow_cv_test
//   (以上两行是一条命令;这里不用反斜杠续行,免得 -Werror=comment 报「多行注释」)
//
// 覆盖:引擎分配→cv::Mat 读写、连续 Mat→包拷贝、**非连续 ROI** Mat→包(验证引擎侧
// 按 strides 拷贝那条路)、以及只读视图读回一致。

#include <algorithm>
#include <cassert>
#include <cstdint>
#include <cstdio>

#include "flow_cv.hpp"

int main() {
  // 1) 引擎分配缓冲 -> cv::Mat 写入 -> 只读视图读回
  {
    cv::Mat m;
    flow::Packet p = flow::NewMatPacket(3, 4, 1, FLOW_DTYPE_U8, &m);
    assert(m.rows == 3 && m.cols == 4 && m.channels() == 1);
    m.at<uint8_t>(1, 2) = 200;
    const cv::Mat v = flow::CvView(p);
    assert(v.at<uint8_t>(1, 2) == 200 && "写进引擎缓冲的值应能从只读视图读回");
  }

  // 2) 连续多通道 Mat -> 包(拷贝)-> 视图,内容逐字节一致
  {
    cv::Mat src(2, 3, CV_8UC3);
    for (int i = 0; i < 2 * 3 * 3; ++i) src.data[i] = static_cast<uint8_t>(i);
    flow::Packet p = flow::PacketFromMat(src);
    const cv::Mat v = flow::CvView(p);
    assert(v.rows == 2 && v.cols == 3 && v.channels() == 3);
    assert(std::equal(src.data, src.data + 18, v.data) && "连续 Mat 拷贝应逐字节一致");
  }

  // 3) 非连续 ROI Mat -> 包:必须按 strides 拷对(引擎侧 N 维 strided copy)
  {
    cv::Mat big(4, 4, CV_8UC1);
    for (int i = 0; i < 16; ++i) big.data[i] = static_cast<uint8_t>(i);
    cv::Mat roi = big(cv::Rect(1, 1, 2, 2));  // 取中间 2x2:step0=4、cols=2 → 非连续
    assert(!roi.isContinuous() && "ROI 应是非连续的");
    flow::Packet p = flow::PacketFromMat(roi);
    const cv::Mat v = flow::CvView(p);
    // big[1..2][1..2] = 值 5,6,9,10
    assert(v.at<uint8_t>(0, 0) == 5 && v.at<uint8_t>(0, 1) == 6);
    assert(v.at<uint8_t>(1, 0) == 9 && v.at<uint8_t>(1, 1) == 10);
  }

  // 4) 可写 CoW:独占的包应能就地改写
  {
    cv::Mat m;
    flow::Packet p = flow::NewMatPacket(2, 2, 1, FLOW_DTYPE_U8, &m);
    m.setTo(0);
    cv::Mat mut;
    FlowStatus st = flow::CvMutable(p, &mut);
    assert(st == FLOW_OK);
    mut.at<uint8_t>(0, 0) = 42;
    const cv::Mat v = flow::CvView(p);
    assert(v.at<uint8_t>(0, 0) == 42 && "独占包的就地改写应可见");
  }

  std::printf("flow_cv.hpp 测试全部通过\n");
  return 0;
}
