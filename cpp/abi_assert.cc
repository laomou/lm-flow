/*
 * abi_assert.cc —— 跨界结构体布局的编译期校验(C++ 侧)。
 *
 * 本文件不含任何运行时逻辑。它把 flow.h 里跨 FFI 结构体的 sizeof/offsetof
 * 钉在一组显式常量上;Rust 侧 tests/abi_layout.rs 钉的是同一组常量。
 * 任何一侧改了字段而忘了同步另一侧,构建就会失败 —— 而不是留到运行期内存错乱。
 *
 * 顺带保持 flow.hpp 的 LMFLOW_REGISTER_KERNEL 宏处于「被编译」状态(编译期覆盖)。
 */
#include <cstddef>
#include <cstdint>

#include "flow.hpp"

/* 只在 64 位平台断言具体数值;其它平台仅校验相对关系 */
#if UINTPTR_MAX == 0xFFFFFFFFFFFFFFFFu

static_assert(sizeof(void*) == 8, "预期 64 位指针");

/* LmflowPacket: payload | type_id | timestamp | owner | drop_fn */
static_assert(sizeof(LmflowPacket) == 40, "LmflowPacket 大小变化 —— 同步 tests/abi_layout.rs");
static_assert(alignof(LmflowPacket) == 8, "LmflowPacket 对齐变化");
static_assert(offsetof(LmflowPacket, payload) == 0, "LmflowPacket::payload 偏移变化");
static_assert(offsetof(LmflowPacket, type_id) == 8, "LmflowPacket::type_id 偏移变化");
static_assert(offsetof(LmflowPacket, timestamp) == 16, "LmflowPacket::timestamp 偏移变化");
static_assert(offsetof(LmflowPacket, owner) == 24, "LmflowPacket::owner 偏移变化");
static_assert(offsetof(LmflowPacket, drop_fn) == 32, "LmflowPacket::drop_fn 偏移变化");

/* LmflowKernelVTable: 6 个函数指针 */
static_assert(sizeof(LmflowKernelVTable) == 48, "LmflowKernelVTable 大小变化");
static_assert(alignof(LmflowKernelVTable) == 8, "LmflowKernelVTable 对齐变化");
static_assert(offsetof(LmflowKernelVTable, create) == 0, "vtable::create 偏移变化");
static_assert(offsetof(LmflowKernelVTable, get_contract) == 8, "vtable::get_contract 偏移变化");
static_assert(offsetof(LmflowKernelVTable, open) == 16, "vtable::open 偏移变化");
static_assert(offsetof(LmflowKernelVTable, process) == 24, "vtable::process 偏移变化");
static_assert(offsetof(LmflowKernelVTable, close) == 32, "vtable::close 偏移变化");
static_assert(offsetof(LmflowKernelVTable, destroy) == 40, "vtable::destroy 偏移变化");

/* LmflowBuffer: data | shape[8] | strides[8] | ndim | dtype | flags | device | reserved[2] */
static_assert(sizeof(LmflowBuffer) == 8 + 64 + 64 + 4 + 4 + 4 + 4 + 16,
              "LmflowBuffer 大小变化 —— 同步 Rust 侧并提升 LMFLOW_ABI_VERSION");
static_assert(alignof(LmflowBuffer) == 8, "LmflowBuffer 对齐变化");
static_assert(offsetof(LmflowBuffer, data) == 0, "LmflowBuffer::data 偏移变化");
static_assert(offsetof(LmflowBuffer, shape) == 8, "LmflowBuffer::shape 偏移变化");
static_assert(offsetof(LmflowBuffer, strides) == 72, "LmflowBuffer::strides 偏移变化");
static_assert(offsetof(LmflowBuffer, ndim) == 136, "LmflowBuffer::ndim 偏移变化");
static_assert(offsetof(LmflowBuffer, dtype) == 140, "LmflowBuffer::dtype 偏移变化");
static_assert(offsetof(LmflowBuffer, flags) == 144, "LmflowBuffer::flags 偏移变化");
static_assert(offsetof(LmflowBuffer, device) == 148, "LmflowBuffer::device 偏移变化");
static_assert(offsetof(LmflowBuffer, reserved) == 152, "LmflowBuffer::reserved 偏移变化");
static_assert(LMFLOW_MAX_DIMS == 8, "LMFLOW_MAX_DIMS 变化会改变 LmflowBuffer 布局");

#endif /* 64-bit */

/* 状态码 / 时间戳哨兵的取值约定 */
static_assert(LMFLOW_OK == 0, "LMFLOW_OK 必须为 0");
static_assert(LMFLOW_TS_UNSET < LMFLOW_TS_UNSTARTED, "时间戳哨兵顺序错误");
static_assert(LMFLOW_TS_UNSTARTED < LMFLOW_TS_PRE_STREAM, "时间戳哨兵顺序错误");
static_assert(LMFLOW_TS_PRE_STREAM < LMFLOW_TS_MIN, "时间戳哨兵顺序错误");
static_assert(LMFLOW_TS_MIN < LMFLOW_TS_MAX, "时间戳哨兵顺序错误");
static_assert(LMFLOW_TS_MAX < LMFLOW_TS_POST_STREAM, "时间戳哨兵顺序错误");
static_assert(LMFLOW_TS_POST_STREAM < LMFLOW_TS_ONE_OVER_POST_STREAM, "时间戳哨兵顺序错误");
static_assert(LMFLOW_TS_ONE_OVER_POST_STREAM < LMFLOW_TS_DONE, "时间戳哨兵顺序错误");

/* ---- 保持糖层宏路径处于编译状态(不参与运行,仅编译期覆盖)---- */
namespace {
class AbiProbeKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) { c.InputSetAny(0); }
  lmflow::Status Process(lmflow::Context&) override { return lmflow::Status::Ok(); }
};
}  // namespace

LMFLOW_REGISTER_KERNEL(AbiProbeKernel)
