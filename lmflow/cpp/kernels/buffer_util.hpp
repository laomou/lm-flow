// buffer_util.hpp —— BUFFER 数值算子的内部工具(**非公开 ABI**,只给 cpp/kernels 用)。
//
// 统一走 double 做 dtype 分派读写:算子逻辑用 double 算,读/写各自按 dtype 转换。
// 写整型时做范围 clamp + 就近取整(不静默回绕)。**不支持 F16**(需 half 转换)——
// `is_math_dtype` 会把它排除,算子应据此报错。约定只处理**连续**缓冲(引擎分配天然连续)。
#ifndef LMFLOW_CPP_KERNELS_BUFFER_UTIL_HPP_
#define LMFLOW_CPP_KERNELS_BUFFER_UTIL_HPP_

#include <cmath>
#include <cstdint>
#include <cstring>
#include <limits>

#include "lmflow/flow.h"

namespace lmflow_bufutil {

inline int64_t elem_count(const LMFlowBuffer& b) {
  int64_t n = 1;
  for (int i = 0; i < b.ndim; ++i) n *= b.shape[i];
  return n;
}

// 行优先连续?strides 按**字节**,最内维步长应 = 元素大小,逐维外推。
inline bool is_contiguous(const LMFlowBuffer& b) {
  size_t es = lmflow_dtype_size(b.dtype);
  if (es == 0 || b.ndim <= 0) return false;
  int64_t expect = static_cast<int64_t>(es);
  for (int i = b.ndim - 1; i >= 0; --i) {
    if (b.strides[i] != expect) return false;
    expect *= b.shape[i];
  }
  return true;
}

// 本工具支持的数值 dtype(F16 除外)。
inline bool is_math_dtype(int32_t dt) {
  switch (dt) {
    case LMFLOW_DTYPE_U8:
    case LMFLOW_DTYPE_I8:
    case LMFLOW_DTYPE_U16:
    case LMFLOW_DTYPE_I16:
    case LMFLOW_DTYPE_I32:
    case LMFLOW_DTYPE_I64:
    case LMFLOW_DTYPE_F32:
    case LMFLOW_DTYPE_F64:
      return true;
    default:  // 含 F16 与未知
      return false;
  }
}

inline double read_f64(const void* p, int32_t dt) {
  switch (dt) {
    case LMFLOW_DTYPE_U8:  return *static_cast<const uint8_t*>(p);
    case LMFLOW_DTYPE_I8:  return *static_cast<const int8_t*>(p);
    case LMFLOW_DTYPE_U16: return *static_cast<const uint16_t*>(p);
    case LMFLOW_DTYPE_I16: return *static_cast<const int16_t*>(p);
    case LMFLOW_DTYPE_I32: return *static_cast<const int32_t*>(p);
    case LMFLOW_DTYPE_I64: return static_cast<double>(*static_cast<const int64_t*>(p));
    case LMFLOW_DTYPE_F32: return *static_cast<const float*>(p);
    case LMFLOW_DTYPE_F64: return *static_cast<const double*>(p);
    default: return 0.0;
  }
}

// 整型目标:范围 clamp + 就近取整,避免溢出回绕 / UB。
template <typename T>
inline T clamp_round(double v) {
  double r = std::nearbyint(v);
  const double lo = static_cast<double>(std::numeric_limits<T>::min());
  const double hi = static_cast<double>(std::numeric_limits<T>::max());
  if (r < lo) r = lo;
  if (r > hi) r = hi;
  return static_cast<T>(r);
}

inline void write_f64(void* p, int32_t dt, double v) {
  switch (dt) {
    case LMFLOW_DTYPE_U8:  *static_cast<uint8_t*>(p)  = clamp_round<uint8_t>(v);  break;
    case LMFLOW_DTYPE_I8:  *static_cast<int8_t*>(p)   = clamp_round<int8_t>(v);   break;
    case LMFLOW_DTYPE_U16: *static_cast<uint16_t*>(p) = clamp_round<uint16_t>(v); break;
    case LMFLOW_DTYPE_I16: *static_cast<int16_t*>(p)  = clamp_round<int16_t>(v);  break;
    case LMFLOW_DTYPE_I32: *static_cast<int32_t*>(p)  = clamp_round<int32_t>(v);  break;
    case LMFLOW_DTYPE_I64: *static_cast<int64_t*>(p)  = clamp_round<int64_t>(v);  break;
    case LMFLOW_DTYPE_F32: *static_cast<float*>(p)    = static_cast<float>(v);    break;
    case LMFLOW_DTYPE_F64: *static_cast<double*>(p)   = v;                        break;
    default: break;
  }
}

// dtype 名(CastKernel 的 options.dtype 用)→ id;未知(含 "f16")返回 -1。
inline int32_t dtype_from_name(const char* s) {
  if (!s) return -1;
  if (!std::strcmp(s, "u8"))  return LMFLOW_DTYPE_U8;
  if (!std::strcmp(s, "i8"))  return LMFLOW_DTYPE_I8;
  if (!std::strcmp(s, "u16")) return LMFLOW_DTYPE_U16;
  if (!std::strcmp(s, "i16")) return LMFLOW_DTYPE_I16;
  if (!std::strcmp(s, "i32")) return LMFLOW_DTYPE_I32;
  if (!std::strcmp(s, "i64")) return LMFLOW_DTYPE_I64;
  if (!std::strcmp(s, "f32")) return LMFLOW_DTYPE_F32;
  if (!std::strcmp(s, "f64")) return LMFLOW_DTYPE_F64;
  return -1;
}

}  // namespace lmflow_bufutil

#endif  // LMFLOW_CPP_KERNELS_BUFFER_UTIL_HPP_
