/*
 * opencv.hpp —— 可选 OpenCV adapter:LMFlowBuffer <-> cv::Mat 零拷贝互转。
 *
 * **不属于 ABI,也不是 core 的依赖。** 只有需要 OpenCV 的算子才 include 本文件;
 * 引擎与 flow.h / flow.hpp 均不依赖 OpenCV,所以没装 OpenCV 也能编译整个 core。
 *
 * 只读 vs 可写(对应 payload 的不可变共享 + CoW 语义):
 *   CvView(pkt)        -> const cv::Mat   只读视图,零拷贝
 *   CvMutable(pkt,&m)  -> 可写 cv::Mat    独占则零拷贝,被共享才复制
 *
 * 就地改写的标准写法(线性管线全程零拷贝):
 *   lmflow::Status Process(lmflow::Context& cc) override {
 *     lmflow::Packet p = cc.TakeInput(0);          // 先取走,否则 CoW 必然复制
 *     cv::Mat m;
 *     if (LMFlowStatus st = lmflow::CvMutable(p, &m)) return st;
 *     cv::GaussianBlur(m, m, {5, 5}, 0);          // 原地
 *     cc.Emit(0, std::move(p));
 *     return lmflow::Status::Ok();
 *   }
 */
#ifndef LMFLOW_OPENCV_HPP_
#define LMFLOW_OPENCV_HPP_

#include <opencv2/core.hpp>
#include <memory>
#include <stdexcept>
#include <string>

#include <lmflow/flow.hpp>

namespace lmflow {

inline int CvDepth(int32_t dtype) {
  switch (dtype) {
    case LMFLOW_DTYPE_U8:  return CV_8U;
    case LMFLOW_DTYPE_I8:  return CV_8S;
    case LMFLOW_DTYPE_U16: return CV_16U;
    case LMFLOW_DTYPE_I16: return CV_16S;
    case LMFLOW_DTYPE_I32: return CV_32S;
    case LMFLOW_DTYPE_F32: return CV_32F;
    case LMFLOW_DTYPE_F64: return CV_64F;
    default: throw std::invalid_argument("flow: dtype has no corresponding cv depth");
  }
}

inline int32_t DtypeFromCv(int cv_depth) {
  switch (cv_depth) {
    case CV_8U:  return LMFLOW_DTYPE_U8;
    case CV_8S:  return LMFLOW_DTYPE_I8;
    case CV_16U: return LMFLOW_DTYPE_U16;
    case CV_16S: return LMFLOW_DTYPE_I16;
    case CV_32S: return LMFLOW_DTYPE_I32;
    case CV_32F: return LMFLOW_DTYPE_F32;
    case CV_64F: return LMFLOW_DTYPE_F64;
    default: throw std::invalid_argument("flow: unsupported cv depth");
  }
}

/// 把 LMFlowBuffer 包成 cv::Mat —— 零拷贝,不接管所有权。
/// 约定:ndim==2 视作单通道 [H,W];ndim==3 视作 [H,W,C]。
/// 返回的 Mat 仅在底层 Packet 存活期间有效。
inline cv::Mat CvWrap(const LMFlowBuffer& b) {
  if (b.ndim != 2 && b.ndim != 3) {
    throw std::invalid_argument("flow: cv::Mat only supports buffers with ndim 2 or 3");
  }
  const int rows = static_cast<int>(b.shape[0]);
  const int cols = static_cast<int>(b.shape[1]);
  const int chan = (b.ndim == 3) ? static_cast<int>(b.shape[2]) : 1;
  return cv::Mat(rows, cols, CV_MAKETYPE(CvDepth(b.dtype), chan), b.data,
                 static_cast<size_t>(b.strides[0]));
}

/// 输入包的**只读**视图(零拷贝)。非缓冲包抛异常(会被糖层蹦床转成错误码)。
inline const cv::Mat CvView(const Packet& pkt) {
  LMFlowBuffer b{};
  if (!pkt.AsBuffer(&b)) throw std::invalid_argument("flow: input is not an LMFlowBuffer packet");
  return CvWrap(b);
}

/// 取得**可写** cv::Mat(CoW):独占零拷贝,被共享才复制。
/// pkt 须为调用方所拥有 —— 典型来自 Context::TakeInput。成功返回 LMFLOW_OK。
inline LMFlowStatus CvMutable(Packet& pkt, cv::Mat* out) {
  LMFlowBuffer b{};
  LMFlowStatus st = pkt.MakeMutableBuffer(&b);
  if (st != LMFLOW_OK) return st;
  *out = CvWrap(b);
  return LMFLOW_OK;
}

/// 让**引擎**分配缓冲,返回 (Packet, 可写 cv::Mat)。产出新图像的推荐路径。
inline Packet NewMatPacket(int rows, int cols, int channels, int32_t dtype, cv::Mat* out) {
  const int64_t shape[3] = {rows, cols, channels};
  LMFlowBuffer b{};
  Packet p = Packet::Adopt(lmflow_packet_new_buffer(3, shape, dtype, LMFLOW_TS_UNSET, &b));
  *out = CvWrap(b);
  return p;
}

/// 便捷:把已有 cv::Mat **拷贝**进新包(src 之后可立即释放)。
inline Packet PacketFromMat(const cv::Mat& m) {
  LMFlowBuffer src{};
  src.data = const_cast<void*>(static_cast<const void*>(m.data));
  src.ndim = 3;
  src.dtype = DtypeFromCv(m.depth());
  src.shape[0] = m.rows;
  src.shape[1] = m.cols;
  src.shape[2] = m.channels();
  src.strides[0] = static_cast<int64_t>(m.step[0]);
  src.strides[1] = static_cast<int64_t>(m.elemSize());
  src.strides[2] = static_cast<int64_t>(m.elemSize1());
  return Packet::Adopt(lmflow_packet_from_buffer(&src, LMFLOW_TS_UNSET));
}

namespace detail {

inline LMFlowBuffer BufferFromMat(const cv::Mat& m, uint32_t flags) {
  if (m.dims != 2) {
    throw std::invalid_argument("flow: only 2-D cv::Mat images can be adopted");
  }
  LMFlowBuffer buffer{};
  buffer.data = m.data;
  buffer.ndim = 3;
  buffer.dtype = DtypeFromCv(m.depth());
  buffer.shape[0] = m.rows;
  buffer.shape[1] = m.cols;
  buffer.shape[2] = m.channels();
  buffer.strides[0] = static_cast<int64_t>(m.step[0]);
  buffer.strides[1] = static_cast<int64_t>(m.elemSize());
  buffer.strides[2] = static_cast<int64_t>(m.elemSize1());
  buffer.flags = flags;
  return buffer;
}

inline void ReleaseAdoptedMat(void* user_data) {
  delete static_cast<cv::Mat*>(user_data);
}

}  // namespace detail

/// 零拷贝接管一个由 OpenCV 引用计数管理的 Mat header。
///
/// 必须显式传 `std::move(mat)`。底层 allocation 可由 ROI/base Mat 共享；
/// Packet 保存自己的 Mat header 引用，最后一个 Packet 引用释放时才减少 OpenCV 引用计数。
/// 引擎把它视为 READONLY，因此 `CvMutable` 会复制后再写，不修改调用方可能仍持有的 alias。
///
/// 用外部裸指针构造且无 OpenCV owner 的 Mat (`mat.u == nullptr`) 无法安全推导释放方式，
/// 请改用 `PacketFromMat` 复制，或直接 `Packet::AdoptBuffer` 并提供真实 owner 回调。
inline Packet AdoptMat(cv::Mat&& mat) {
  if (mat.data && mat.u == nullptr) {
    throw std::invalid_argument(
        "flow: cannot adopt a cv::Mat without an OpenCV-owned allocation; "
        "use PacketFromMat to copy or Packet::AdoptBuffer with an explicit owner");
  }
  auto owner = std::make_unique<cv::Mat>(std::move(mat));
  const LMFlowBuffer buffer = detail::BufferFromMat(*owner, LMFLOW_BUF_FLAG_READONLY);
  Packet packet = Packet::AdoptBuffer(buffer, &detail::ReleaseAdoptedMat, owner.get());
  if (packet.IsEmpty()) {
    const char* detail = lmflow_last_error();
    throw std::runtime_error(std::string("flow: AdoptMat failed") +
                             (detail && *detail ? ": " + std::string(detail) : ""));
  }
  owner.release();
  return packet;
}

}  // namespace lmflow

#endif  // LMFLOW_OPENCV_HPP_
