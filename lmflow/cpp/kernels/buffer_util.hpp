// buffer_util.hpp —— BUFFER 数值算子的内部工具(**非公开 ABI**,只给 cpp/kernels 用)。
//
// 统一走 double 做 dtype 分派读写:算子逻辑用 double 算,读/写各自按 dtype 转换。
// 写整型时做范围 clamp + 就近取整(不静默回绕)。约定只处理**连续**缓冲(引擎分配天然连续)。
//
// F16 走**自带的软件转换**(下面 `half_to_float` / `f64_to_half`),不用 `_Float16`、
// 也不用 F16C/NEON 内建:前者不是所有目标编译器都有(MSVC 就没有可移植的 half 类型),
// 后者要按架构分派 + 运行期探测。张量前处理不在最内层推理热路径上,这点转换成本换来
// 「任意编译器/架构上行为逐位一致」是值得的 —— 而且它让 F16 的舍入结果可被测试钉死。
#ifndef LMFLOW_CPP_KERNELS_BUFFER_UTIL_HPP_
#define LMFLOW_CPP_KERNELS_BUFFER_UTIL_HPP_

#include <cmath>
#include <cstdint>
#include <cstring>
#include <limits>

#include "lmflow/flow.h"

namespace lmflow_bufutil {

// ---------------------------------------------------------------- F16(IEEE 754 binary16)
// 布局:1 位符号 + 5 位阶码(偏置 15)+ 10 位尾数。规格数 2^-14 ~ 65504,
// 非规格数下探到 2^-24。inf/NaN 的阶码全 1。

/// binary16 → float。**精确无损**(float 的阶码与尾数都足够宽),故再隐式加宽到
/// double 也精确。非规格数需先归一化;inf/NaN 保留尾数载荷。
inline float half_to_float(uint16_t h) {
  const uint32_t sign = static_cast<uint32_t>(h >> 15) << 31;
  uint32_t exp = (h >> 10) & 0x1Fu;
  uint32_t man = h & 0x3FFu;
  uint32_t bits;

  if (exp == 0) {
    if (man == 0) {
      bits = sign;  // ±0
    } else {
      // 非规格数:值 = man * 2^-24。左移到 bit10 置位以取得隐含 1,阶码同步回退。
      // 推导:移 k 位后 值 = (1 + frac/1024) * 2^(-14-k) → 偏置阶码 = 113 - k。
      uint32_t e = 113;
      while ((man & 0x400u) == 0) {
        man <<= 1;
        --e;
      }
      man &= 0x3FFu;
      bits = sign | (e << 23) | (man << 13);
    }
  } else if (exp == 31) {
    bits = sign | 0x7F800000u | (man << 13);  // inf(man==0)或 NaN(保留载荷)
  } else {
    bits = sign | ((exp - 15 + 127) << 23) | (man << 13);
  }

  float f;
  std::memcpy(&f, &bits, sizeof(f));
  return f;
}

/// double → binary16,**就近取整、平局取偶**(与 IEEE 默认舍入一致)。
/// 直接从 double 位模式做,不经 float 中转 —— 否则会**双重舍入**(double→float→half
/// 各取整一次,极少数入参会偏一个 ulp)。
inline uint16_t f64_to_half(double d) {
  uint64_t x;
  std::memcpy(&x, &d, sizeof(x));

  const uint32_t sign = static_cast<uint32_t>((x >> 48) & 0x8000u);
  const int32_t be = static_cast<int32_t>((x >> 52) & 0x7FFu);  // 偏置阶码
  uint64_t man = x & 0xFFFFFFFFFFFFFull;                        // 52 位尾数

  if (be == 0x7FF) {                                            // inf / NaN
    return static_cast<uint16_t>(sign | (man ? 0x7E00u : 0x7C00u));
  }
  if (be == 0) {
    return static_cast<uint16_t>(sign);  // double 的 0 与非规格数(< 2^-1022)一律归 ±0
  }

  const int32_t exp = be - 1023;
  if (exp > 15) {
    return static_cast<uint16_t>(sign | 0x7C00u);  // 超出 half 范围 → ±inf
  }

  if (exp >= -14) {  // half 规格数
    const uint32_t h_man = static_cast<uint32_t>(man >> 42);  // 52 - 10
    const uint64_t rem = man & ((1ull << 42) - 1);
    const uint64_t halfp = 1ull << 41;
    uint16_t h = static_cast<uint16_t>(sign | (static_cast<uint32_t>(exp + 15) << 10) | h_man);
    // 进位可能溢进阶码 —— 那正是想要的(尾数全 1 再进位 = 下一个阶码,
    // 阶码 30 再进位自然得到 0x7C00 = inf)。
    if (rem > halfp || (rem == halfp && (h_man & 1u))) ++h;
    return h;
  }

  if (exp < -25) {
    return static_cast<uint16_t>(sign);  // 比最小非规格数的一半还小 → ±0
  }

  // half 非规格数:值表示成 h_man * 2^-24,阶码域留 0。
  man |= 1ull << 52;  // 补回隐含 1
  const uint32_t total = static_cast<uint32_t>(42 + (-14 - exp));  // 43..53
  uint32_t h_man = static_cast<uint32_t>(man >> total);
  const uint64_t rem = man & ((1ull << total) - 1);
  const uint64_t halfp = 1ull << (total - 1);
  if (rem > halfp || (rem == halfp && (h_man & 1u))) ++h_man;
  return static_cast<uint16_t>(sign | h_man);
}

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

// 本工具支持的数值 dtype。
inline bool is_math_dtype(int32_t dt) {
  switch (dt) {
    case LMFLOW_DTYPE_U8:
    case LMFLOW_DTYPE_I8:
    case LMFLOW_DTYPE_U16:
    case LMFLOW_DTYPE_I16:
    case LMFLOW_DTYPE_I32:
    case LMFLOW_DTYPE_I64:
    case LMFLOW_DTYPE_F16:
    case LMFLOW_DTYPE_F32:
    case LMFLOW_DTYPE_F64:
      return true;
    default:  // 未知 dtype
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
    case LMFLOW_DTYPE_F16: return half_to_float(*static_cast<const uint16_t*>(p));
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
    case LMFLOW_DTYPE_F16: *static_cast<uint16_t*>(p) = f64_to_half(v);           break;
    case LMFLOW_DTYPE_F32: *static_cast<float*>(p)    = static_cast<float>(v);    break;
    case LMFLOW_DTYPE_F64: *static_cast<double*>(p)   = v;                        break;
    default: break;
  }
}

// dtype 名(CastKernel 的 options.dtype 用)→ id;未知返回 -1。
inline int32_t dtype_from_name(const char* s) {
  if (!s) return -1;
  if (!std::strcmp(s, "u8"))  return LMFLOW_DTYPE_U8;
  if (!std::strcmp(s, "i8"))  return LMFLOW_DTYPE_I8;
  if (!std::strcmp(s, "u16")) return LMFLOW_DTYPE_U16;
  if (!std::strcmp(s, "i16")) return LMFLOW_DTYPE_I16;
  if (!std::strcmp(s, "i32")) return LMFLOW_DTYPE_I32;
  if (!std::strcmp(s, "i64")) return LMFLOW_DTYPE_I64;
  if (!std::strcmp(s, "f16")) return LMFLOW_DTYPE_F16;
  if (!std::strcmp(s, "f32")) return LMFLOW_DTYPE_F32;
  if (!std::strcmp(s, "f64")) return LMFLOW_DTYPE_F64;
  return -1;
}

}  // namespace lmflow_bufutil

#endif  // LMFLOW_CPP_KERNELS_BUFFER_UTIL_HPP_
