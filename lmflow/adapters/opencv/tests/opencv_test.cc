// lmflow OpenCV adapter 单元测试 —— 需要 OpenCV,并链接引擎库。
//
//   g++ -std=c++17 -Iinclude adapters/opencv/tests/opencv_test.cc target/release/liblmflow.a
//       $(pkg-config --cflags --libs opencv4) -lpthread -ldl -lm -o lmflow_opencv_test
//   (以上两行是一条命令;这里不用反斜杠续行,免得 -Werror=comment 报「多行注释」)
//
// 覆盖:引擎分配、复制输入、零拷贝 adopt、非连续 ROI、CoW 与 owner 生命周期。

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>

#include "lmflow/opencv.hpp"

#define CHECK(condition)                                                        \
  do {                                                                          \
    if (!(condition)) {                                                         \
      std::fprintf(stderr, "CHECK failed at %s:%d: %s\n", __FILE__, __LINE__, \
                   #condition);                                                 \
      return EXIT_FAILURE;                                                      \
    }                                                                           \
  } while (false)

int main() {
  // 1) 引擎分配缓冲 -> cv::Mat 写入 -> 只读视图读回
  {
    cv::Mat m;
    lmflow::Packet p = lmflow::NewMatPacket(3, 4, 1, LMFLOW_DTYPE_U8, &m);
    CHECK(m.rows == 3 && m.cols == 4 && m.channels() == 1);
    m.at<uint8_t>(1, 2) = 200;
    const cv::Mat v = lmflow::CvView(p);
    CHECK(v.at<uint8_t>(1, 2) == 200);
  }

  // 2) 连续多通道 Mat -> 包(拷贝)-> 视图,内容逐字节一致
  {
    cv::Mat src(2, 3, CV_8UC3);
    for (int i = 0; i < 2 * 3 * 3; ++i) src.data[i] = static_cast<uint8_t>(i);
    lmflow::Packet p = lmflow::PacketFromMat(src);
    const cv::Mat v = lmflow::CvView(p);
    CHECK(v.rows == 2 && v.cols == 3 && v.channels() == 3);
    CHECK(std::equal(src.data, src.data + 18, v.data));
  }

  // 3) 非连续 ROI Mat -> 包:必须按 strides 拷对(引擎侧 N 维 strided copy)
  {
    cv::Mat big(4, 4, CV_8UC1);
    for (int i = 0; i < 16; ++i) big.data[i] = static_cast<uint8_t>(i);
    cv::Mat roi = big(cv::Rect(1, 1, 2, 2));  // 取中间 2x2:step0=4、cols=2 → 非连续
    CHECK(!roi.isContinuous());
    lmflow::Packet p = lmflow::PacketFromMat(roi);
    const cv::Mat v = lmflow::CvView(p);
    // big[1..2][1..2] = 值 5,6,9,10
    CHECK(v.at<uint8_t>(0, 0) == 5 && v.at<uint8_t>(0, 1) == 6);
    CHECK(v.at<uint8_t>(1, 0) == 9 && v.at<uint8_t>(1, 1) == 10);
  }

  // 4) 可写 CoW:独占的包应能就地改写
  {
    cv::Mat m;
    lmflow::Packet p = lmflow::NewMatPacket(2, 2, 1, LMFLOW_DTYPE_U8, &m);
    m.setTo(0);
    cv::Mat mut;
    LMFlowStatus st = lmflow::CvMutable(p, &mut);
    CHECK(st == LMFLOW_OK);
    mut.at<uint8_t>(0, 0) = 42;
    const cv::Mat v = lmflow::CvView(p);
    CHECK(v.at<uint8_t>(0, 0) == 42);
  }

  // 5) 连续 Mat 零拷贝 adopt:地址相同,原 header 释放后 Packet 仍持有 allocation
  {
    cv::Mat src(2, 3, CV_8UC3);
    for (int i = 0; i < 18; ++i) src.data[i] = static_cast<uint8_t>(i + 1);
    const uint8_t* address = src.data;
    lmflow::Packet p = lmflow::AdoptMat(std::move(src));
    CHECK(src.empty());
    const cv::Mat v = lmflow::CvView(p);
    CHECK(v.data == address);
    CHECK(v.at<cv::Vec3b>(1, 2)[2] == 18);
  }

  // 6) ROI 零拷贝:保留 data 起点和行步长;释放 base/roi header 后 Packet 仍有效
  {
    cv::Mat big(4, 5, CV_8UC1);
    for (int i = 0; i < 20; ++i) big.data[i] = static_cast<uint8_t>(i);
    cv::Mat roi = big(cv::Rect(1, 1, 3, 2));
    const uint8_t* address = roi.data;
    const size_t row_step = roi.step[0];
    lmflow::Packet p = lmflow::AdoptMat(std::move(roi));
    big.release();
    const cv::Mat v = lmflow::CvView(p);
    CHECK(v.data == address);
    CHECK(v.step[0] == row_step);
    CHECK(v.at<uint8_t>(0, 0) == 6 && v.at<uint8_t>(1, 2) == 13);
  }

  // 7) adopted Mat 是 READONLY:请求可写视图必须 CoW,不能污染仍存在的 alias
  {
    cv::Mat src(2, 2, CV_8UC1);
    src.setTo(7);
    cv::Mat alias = src;
    CHECK(alias.u && alias.u->refcount == 2);
    const uint8_t* source_address = src.data;
    {
      lmflow::Packet p = lmflow::AdoptMat(std::move(src));
      CHECK(alias.u->refcount == 2);
      cv::Mat mut;
      CHECK(lmflow::CvMutable(p, &mut) == LMFLOW_OK);
      CHECK(mut.data != source_address);
      CHECK(alias.u->refcount == 1);
      mut.at<uint8_t>(0, 0) = 99;
      CHECK(alias.at<uint8_t>(0, 0) == 7);
      CHECK(lmflow::CvView(p).at<uint8_t>(0, 0) == 99);
    }
    CHECK(alias.u->refcount == 1);
  }

  // 8) 外部裸指针 Mat 没有 OpenCV owner,必须拒绝隐式 adopt
  {
    uint8_t bytes[4] = {1, 2, 3, 4};
    cv::Mat borrowed(2, 2, CV_8UC1, bytes);
    bool rejected = false;
    try {
      (void)lmflow::AdoptMat(std::move(borrowed));
    } catch (const std::invalid_argument&) {
      rejected = true;
    }
    CHECK(rejected);
  }

  std::printf("all lmflow OpenCV adapter tests passed\n");
  return EXIT_SUCCESS;
}
