/*
 * abi_assert.cc —— 跨界结构体布局的编译期校验(C++ 侧)。
 *
 * 本文件不含任何运行时逻辑。它把 flow.h 里跨 FFI 结构体的 sizeof/offsetof
 * 钉在一组显式常量上;Rust 侧 tests/abi_layout.rs 钉的是同一组常量。
 * 任何一侧改了字段而忘了同步另一侧,构建就会失败 —— 而不是留到运行期内存错乱。
 *
 * 顺带保持 flow.hpp 的 FLOW_REGISTER_KERNEL 宏处于「被编译」状态(编译期覆盖)。
 */
#include <cstddef>
#include <cstdint>

#include "flow.hpp"

/* 只在 64 位平台断言具体数值;其它平台仅校验相对关系 */
#if UINTPTR_MAX == 0xFFFFFFFFFFFFFFFFu

static_assert(sizeof(void*) == 8, "预期 64 位指针");

/* FlowPacket: payload | type_id | timestamp | owner | drop_fn */
static_assert(sizeof(FlowPacket) == 40, "FlowPacket 大小变化 —— 同步 tests/abi_layout.rs");
static_assert(alignof(FlowPacket) == 8, "FlowPacket 对齐变化");
static_assert(offsetof(FlowPacket, payload) == 0, "FlowPacket::payload 偏移变化");
static_assert(offsetof(FlowPacket, type_id) == 8, "FlowPacket::type_id 偏移变化");
static_assert(offsetof(FlowPacket, timestamp) == 16, "FlowPacket::timestamp 偏移变化");
static_assert(offsetof(FlowPacket, owner) == 24, "FlowPacket::owner 偏移变化");
static_assert(offsetof(FlowPacket, drop_fn) == 32, "FlowPacket::drop_fn 偏移变化");

/* FlowKernelVTable: 6 个函数指针 */
static_assert(sizeof(FlowKernelVTable) == 48, "FlowKernelVTable 大小变化");
static_assert(alignof(FlowKernelVTable) == 8, "FlowKernelVTable 对齐变化");
static_assert(offsetof(FlowKernelVTable, create) == 0, "vtable::create 偏移变化");
static_assert(offsetof(FlowKernelVTable, get_contract) == 8, "vtable::get_contract 偏移变化");
static_assert(offsetof(FlowKernelVTable, open) == 16, "vtable::open 偏移变化");
static_assert(offsetof(FlowKernelVTable, process) == 24, "vtable::process 偏移变化");
static_assert(offsetof(FlowKernelVTable, close) == 32, "vtable::close 偏移变化");
static_assert(offsetof(FlowKernelVTable, destroy) == 40, "vtable::destroy 偏移变化");

/* FlowBuffer: data | shape[8] | strides[8] | ndim | dtype | flags | device | reserved[2] */
static_assert(sizeof(FlowBuffer) == 8 + 64 + 64 + 4 + 4 + 4 + 4 + 16,
              "FlowBuffer 大小变化 —— 同步 Rust 侧并提升 FLOW_ABI_VERSION");
static_assert(alignof(FlowBuffer) == 8, "FlowBuffer 对齐变化");
static_assert(offsetof(FlowBuffer, data) == 0, "FlowBuffer::data 偏移变化");
static_assert(offsetof(FlowBuffer, shape) == 8, "FlowBuffer::shape 偏移变化");
static_assert(offsetof(FlowBuffer, strides) == 72, "FlowBuffer::strides 偏移变化");
static_assert(offsetof(FlowBuffer, ndim) == 136, "FlowBuffer::ndim 偏移变化");
static_assert(offsetof(FlowBuffer, dtype) == 140, "FlowBuffer::dtype 偏移变化");
static_assert(offsetof(FlowBuffer, flags) == 144, "FlowBuffer::flags 偏移变化");
static_assert(offsetof(FlowBuffer, device) == 148, "FlowBuffer::device 偏移变化");
static_assert(offsetof(FlowBuffer, reserved) == 152, "FlowBuffer::reserved 偏移变化");
static_assert(FLOW_MAX_DIMS == 8, "FLOW_MAX_DIMS 变化会改变 FlowBuffer 布局");

#endif /* 64-bit */

/* 状态码 / 时间戳哨兵的取值约定 */
static_assert(FLOW_OK == 0, "FLOW_OK 必须为 0");
static_assert(FLOW_TS_UNSET < FLOW_TS_UNSTARTED, "时间戳哨兵顺序错误");
static_assert(FLOW_TS_UNSTARTED < FLOW_TS_PRE_STREAM, "时间戳哨兵顺序错误");
static_assert(FLOW_TS_PRE_STREAM < FLOW_TS_MIN, "时间戳哨兵顺序错误");
static_assert(FLOW_TS_MIN < FLOW_TS_MAX, "时间戳哨兵顺序错误");
static_assert(FLOW_TS_MAX < FLOW_TS_POST_STREAM, "时间戳哨兵顺序错误");
static_assert(FLOW_TS_POST_STREAM < FLOW_TS_ONE_OVER_POST_STREAM, "时间戳哨兵顺序错误");
static_assert(FLOW_TS_ONE_OVER_POST_STREAM < FLOW_TS_DONE, "时间戳哨兵顺序错误");

/* ---- 保持糖层宏路径处于编译状态(不参与运行,仅编译期覆盖)---- */
namespace {
class AbiProbeKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) { c.InputSetAny(0); }
  flow::Status Process(flow::Context&) override { return flow::Status::Ok(); }
};
}  // namespace

FLOW_REGISTER_KERNEL(AbiProbeKernel)
