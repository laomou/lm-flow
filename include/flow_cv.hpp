/*
 * flow_cv.hpp —— 可选头:FlowBuffer <-> cv::Mat 零拷贝互转。
 *
 * **不属于 ABI,也不是 core 的依赖。** 只有需要 OpenCV 的算子才 include 本文件;
 * 引擎与 flow.h / flow.hpp 均不依赖 OpenCV,所以没装 OpenCV 也能编译整个 core。
 *
 * 只读 vs 可写(对应 payload 的不可变共享 + CoW 语义):
 *   CvView(pkt)        -> const cv::Mat   只读视图,零拷贝
 *   CvMutable(pkt,&m)  -> 可写 cv::Mat    独占则零拷贝,被共享才复制
 *
 * 就地改写的标准写法(线性管线全程零拷贝):
 *   flow::Status Process(flow::Context& cc) override {
 *     flow::Packet p = cc.TakeInput(0);          // 先取走,否则 CoW 必然复制
 *     cv::Mat m;
 *     if (FlowStatus st = flow::CvMutable(p, &m)) return st;
 *     cv::GaussianBlur(m, m, {5, 5}, 0);          // 原地
 *     cc.Emit(0, std::move(p));
 *     return flow::Status::Ok();
 *   }
 */
#ifndef FLOW_CV_HPP_
#define FLOW_CV_HPP_

#include <opencv2/core.hpp>
#include <stdexcept>

#include "flow.hpp"

namespace flow {

inline int CvDepth(int32_t dtype) {
  switch (dtype) {
    case FLOW_DTYPE_U8:  return CV_8U;
    case FLOW_DTYPE_I8:  return CV_8S;
    case FLOW_DTYPE_U16: return CV_16U;
    case FLOW_DTYPE_I16: return CV_16S;
    case FLOW_DTYPE_I32: return CV_32S;
    case FLOW_DTYPE_F32: return CV_32F;
    case FLOW_DTYPE_F64: return CV_64F;
    default: throw std::invalid_argument("flow: dtype 无对应的 cv depth");
  }
}

inline int32_t DtypeFromCv(int cv_depth) {
  switch (cv_depth) {
    case CV_8U:  return FLOW_DTYPE_U8;
    case CV_8S:  return FLOW_DTYPE_I8;
    case CV_16U: return FLOW_DTYPE_U16;
    case CV_16S: return FLOW_DTYPE_I16;
    case CV_32S: return FLOW_DTYPE_I32;
    case CV_32F: return FLOW_DTYPE_F32;
    case CV_64F: return FLOW_DTYPE_F64;
    default: throw std::invalid_argument("flow: 不支持的 cv depth");
  }
}

/// 把 FlowBuffer 包成 cv::Mat —— 零拷贝,不接管所有权。
/// 约定:ndim==2 视作单通道 [H,W];ndim==3 视作 [H,W,C]。
/// 返回的 Mat 仅在底层 Packet 存活期间有效。
inline cv::Mat CvWrap(const FlowBuffer& b) {
  if (b.ndim != 2 && b.ndim != 3) {
    throw std::invalid_argument("flow: cv::Mat 只支持 ndim 为 2 或 3 的缓冲");
  }
  const int rows = static_cast<int>(b.shape[0]);
  const int cols = static_cast<int>(b.shape[1]);
  const int chan = (b.ndim == 3) ? static_cast<int>(b.shape[2]) : 1;
  return cv::Mat(rows, cols, CV_MAKETYPE(CvDepth(b.dtype), chan), b.data,
                 static_cast<size_t>(b.strides[0]));
}

/// 输入包的**只读**视图(零拷贝)。非缓冲包抛异常(会被糖层蹦床转成错误码)。
inline const cv::Mat CvView(const Packet& pkt) {
  FlowBuffer b{};
  if (!pkt.AsBuffer(&b)) throw std::invalid_argument("flow: 输入不是 FlowBuffer 包");
  return CvWrap(b);
}

/// 取得**可写** cv::Mat(CoW):独占零拷贝,被共享才复制。
/// pkt 须为调用方所拥有 —— 典型来自 Context::TakeInput。成功返回 FLOW_OK。
inline FlowStatus CvMutable(Packet& pkt, cv::Mat* out) {
  FlowBuffer b{};
  FlowStatus st = pkt.MakeMutableBuffer(&b);
  if (st != FLOW_OK) return st;
  *out = CvWrap(b);
  return FLOW_OK;
}

/// 让**引擎**分配缓冲,返回 (Packet, 可写 cv::Mat)。产出新图像的推荐路径。
inline Packet NewMatPacket(int rows, int cols, int channels, int32_t dtype, cv::Mat* out) {
  const int64_t shape[3] = {rows, cols, channels};
  FlowBuffer b{};
  Packet p = Packet::Adopt(flow_packet_new_buffer(3, shape, dtype, FLOW_TS_UNSET, &b));
  *out = CvWrap(b);
  return p;
}

/// 便捷:把已有 cv::Mat **拷贝**进新包(src 之后可立即释放)。
inline Packet PacketFromMat(const cv::Mat& m) {
  FlowBuffer src{};
  src.data = const_cast<void*>(static_cast<const void*>(m.data));
  src.ndim = 3;
  src.dtype = DtypeFromCv(m.depth());
  src.shape[0] = m.rows;
  src.shape[1] = m.cols;
  src.shape[2] = m.channels();
  src.strides[0] = static_cast<int64_t>(m.step[0]);
  src.strides[1] = static_cast<int64_t>(m.elemSize());
  src.strides[2] = static_cast<int64_t>(m.elemSize1());
  return Packet::Adopt(flow_packet_from_buffer(&src, FLOW_TS_UNSET));
}

}  // namespace flow

#endif  // FLOW_CV_HPP_
