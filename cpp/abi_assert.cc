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

/* LMFlowPacket: payload | type_id | timestamp | owner | drop_fn */
static_assert(sizeof(LMFlowPacket) == 40, "LMFlowPacket 大小变化 —— 同步 tests/abi_layout.rs");
static_assert(alignof(LMFlowPacket) == 8, "LMFlowPacket 对齐变化");
static_assert(offsetof(LMFlowPacket, payload) == 0, "LMFlowPacket::payload 偏移变化");
static_assert(offsetof(LMFlowPacket, type_id) == 8, "LMFlowPacket::type_id 偏移变化");
static_assert(offsetof(LMFlowPacket, timestamp) == 16, "LMFlowPacket::timestamp 偏移变化");
static_assert(offsetof(LMFlowPacket, owner) == 24, "LMFlowPacket::owner 偏移变化");
static_assert(offsetof(LMFlowPacket, drop_fn) == 32, "LMFlowPacket::drop_fn 偏移变化");

/* LMFlowKernelVTable: 6 个函数指针 */
static_assert(sizeof(LMFlowKernelVTable) == 48, "LMFlowKernelVTable 大小变化");
static_assert(alignof(LMFlowKernelVTable) == 8, "LMFlowKernelVTable 对齐变化");
static_assert(offsetof(LMFlowKernelVTable, create) == 0, "vtable::create 偏移变化");
static_assert(offsetof(LMFlowKernelVTable, get_contract) == 8, "vtable::get_contract 偏移变化");
static_assert(offsetof(LMFlowKernelVTable, open) == 16, "vtable::open 偏移变化");
static_assert(offsetof(LMFlowKernelVTable, process) == 24, "vtable::process 偏移变化");
static_assert(offsetof(LMFlowKernelVTable, close) == 32, "vtable::close 偏移变化");
static_assert(offsetof(LMFlowKernelVTable, destroy) == 40, "vtable::destroy 偏移变化");

/* LMFlowBuffer: data | shape[8] | strides[8] | ndim | dtype | flags | device | reserved[2] */
static_assert(sizeof(LMFlowBuffer) == 8 + 64 + 64 + 4 + 4 + 4 + 4 + 16,
              "LMFlowBuffer 大小变化 —— 同步 Rust 侧并提升 LMFLOW_ABI_VERSION");
static_assert(alignof(LMFlowBuffer) == 8, "LMFlowBuffer 对齐变化");
static_assert(offsetof(LMFlowBuffer, data) == 0, "LMFlowBuffer::data 偏移变化");
static_assert(offsetof(LMFlowBuffer, shape) == 8, "LMFlowBuffer::shape 偏移变化");
static_assert(offsetof(LMFlowBuffer, strides) == 72, "LMFlowBuffer::strides 偏移变化");
static_assert(offsetof(LMFlowBuffer, ndim) == 136, "LMFlowBuffer::ndim 偏移变化");
static_assert(offsetof(LMFlowBuffer, dtype) == 140, "LMFlowBuffer::dtype 偏移变化");
static_assert(offsetof(LMFlowBuffer, flags) == 144, "LMFlowBuffer::flags 偏移变化");
static_assert(offsetof(LMFlowBuffer, device) == 148, "LMFlowBuffer::device 偏移变化");
static_assert(offsetof(LMFlowBuffer, reserved) == 152, "LMFlowBuffer::reserved 偏移变化");
static_assert(LMFLOW_MAX_DIMS == 8, "LMFLOW_MAX_DIMS 变化会改变 LMFlowBuffer 布局");

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
