// buffer_util.hpp 的 F16 软件转换单元测试 —— 纯头,不链引擎。
//
// 为什么要独立钉住:F16 转换是**自己实现的**(不用 `_Float16`,因为它不是所有目标
// 编译器都有 —— MSVC 就没有可移植的 half 类型),所以正确性没有编译器兜底,必须由
// 测试来保证。本文件里的期望值全是**硬编码的 IEEE 754 binary16 位模式**,不与任何
// 编译器内建类型对照 —— 这样它在 MSVC / arm64 / 交叉编译下同样有效。
//
// 覆盖:规格数、非规格数、零与负零、inf/NaN、上溢、下溢、以及**平局取偶**
// (最容易写错、也最容易被「看起来对」的实现蒙过去的一类)。
//
//   g++ -std=c++17 -Wall -Wextra -Werror -Iinclude -Icpp/kernels
//       cpp/tests/buffer_util_test.cc -o buffer_util_test

#include <cassert>
#include <cmath>
#include <cstdio>
#include <cstdint>

#include "buffer_util.hpp"

using lmflow_bufutil::f64_to_half;
using lmflow_bufutil::half_to_float;

namespace {

void test_half_to_float_exact() {
  // 这些值在 binary16 里都是精确可表示的,故等号比较是合法的(不是浮点容差问题)。
  assert(half_to_float(0x0000) == 0.0f);
  assert(half_to_float(0x3C00) == 1.0f);
  assert(half_to_float(0xBC00) == -1.0f);
  assert(half_to_float(0x4000) == 2.0f);
  assert(half_to_float(0x7BFF) == 65504.0f);  // half 最大有限值
  assert(half_to_float(0x0400) == 6.103515625e-05f);              // 2^-14,最小规格数
  assert(half_to_float(0x0001) == 5.9604644775390625e-08f);       // 2^-24,最小非规格数
  assert(half_to_float(0x03FF) == 6.0975551605224609e-05f);       // 最大非规格数
  assert(half_to_float(0x3555) == 0.333251953125f);               // 1/3 的最近 half

  // 负零必须保号:值等于 0 但符号位为 1(用 signbit 才验得出来,== 会漏)。
  const float nz = half_to_float(0x8000);
  assert(nz == 0.0f && std::signbit(nz));

  assert(std::isinf(half_to_float(0x7C00)) && half_to_float(0x7C00) > 0);
  assert(std::isinf(half_to_float(0xFC00)) && half_to_float(0xFC00) < 0);
  assert(std::isnan(half_to_float(0x7E00)));
  assert(std::isnan(half_to_float(0xFFFF)));
}

void test_f64_to_half_values() {
  assert(f64_to_half(1.0) == 0x3C00);
  assert(f64_to_half(-2.0) == 0xC000);
  assert(f64_to_half(65504.0) == 0x7BFF);   // 恰好最大有限值
  assert(f64_to_half(1.0 / 3.0) == 0x3555);
  assert(f64_to_half(0.1) == 0x2E66);
  assert(f64_to_half(std::ldexp(1.0, -14)) == 0x0400);  // 最小规格数
  assert(f64_to_half(std::ldexp(1.0, -24)) == 0x0001);  // 最小非规格数

  // 零与负零都要保号。
  assert(f64_to_half(0.0) == 0x0000);
  assert(f64_to_half(-0.0) == 0x8000);
}

void test_f64_to_half_overflow_underflow() {
  // 65519 仍舍入到最大有限值;65520 是「进位溢进阶码」的临界点 → +inf。
  // 这一对专门守着 `exp > 15` 的粗判会漏掉的那种情况。
  assert(f64_to_half(65519.0) == 0x7BFF);
  assert(f64_to_half(65520.0) == 0x7C00);
  assert(f64_to_half(1e300) == 0x7C00);
  assert(f64_to_half(-1e300) == 0xFC00);

  // 下溢:2^-25 正好是最小非规格数的一半 → 平局取偶 → 0(不是 1)。
  assert(f64_to_half(std::ldexp(1.0, -25)) == 0x0000);
  assert(f64_to_half(1.5 * std::ldexp(1.0, -25)) == 0x0001);  // 过半 → 进位
  assert(f64_to_half(std::ldexp(1.0, -26)) == 0x0000);
  assert(f64_to_half(-std::ldexp(1.0, -30)) == 0x8000);  // 下溢到 -0,保号

  assert(f64_to_half(INFINITY) == 0x7C00);
  assert(f64_to_half(-INFINITY) == 0xFC00);
  assert(f64_to_half(NAN) == 0x7E00);  // 统一收敛到 quiet NaN
}

void test_ties_to_even() {
  // 两个相邻 half 的**精确中点**:结果必须取尾数为偶的那一侧。
  // 0x3C00 尾数偶 → 中点向下;0x3C02 尾数偶 → 中点向上。
  // 若实现写成「一律向上」或「一律截断」,这两条必有一条挂。
  const double lo = half_to_float(0x3C00), mid1 = (lo + half_to_float(0x3C01)) / 2.0;
  assert(f64_to_half(mid1) == 0x3C00);

  const double a = half_to_float(0x3C01), mid2 = (a + half_to_float(0x3C02)) / 2.0;
  assert(f64_to_half(mid2) == 0x3C02);
}

void test_roundtrip_exhaustive() {
  // 全部 65536 个位模式:half → double → half 必须逐位回到原值。
  // NaN 除外(载荷会被统一成 quiet NaN,这是有意的)。
  int checked = 0;
  for (uint32_t i = 0; i < 65536; ++i) {
    const uint16_t h = static_cast<uint16_t>(i);
    const float f = half_to_float(h);
    if (std::isnan(f)) continue;
    const uint16_t back = f64_to_half(static_cast<double>(f));
    assert(back == h);
    ++checked;
  }
  assert(checked == 65536 - 2046);  // 2046 = 两个符号各 1023 个 NaN 载荷
}

void test_dtype_plumbing() {
  // F16 已接入 dtype 分派:名字、是否可算、读写往返。
  assert(lmflow_bufutil::dtype_from_name("f16") == LMFLOW_DTYPE_F16);
  assert(lmflow_bufutil::is_math_dtype(LMFLOW_DTYPE_F16));

  uint16_t storage = 0;
  lmflow_bufutil::write_f64(&storage, LMFLOW_DTYPE_F16, 0.5);
  assert(storage == 0x3800);  // 0.5 的 binary16
  assert(lmflow_bufutil::read_f64(&storage, LMFLOW_DTYPE_F16) == 0.5);

  // 写入超出 half 范围的值 → +inf(而不是回绕成某个有限值)。
  lmflow_bufutil::write_f64(&storage, LMFLOW_DTYPE_F16, 1e30);
  assert(storage == 0x7C00);
}

}  // namespace

int main() {
  test_half_to_float_exact();
  test_f64_to_half_values();
  test_f64_to_half_overflow_underflow();
  test_ties_to_even();
  test_roundtrip_exhaustive();
  test_dtype_plumbing();
  std::printf("buffer_util_test: all F16 conversion assertions passed\n");
  return 0;
}
