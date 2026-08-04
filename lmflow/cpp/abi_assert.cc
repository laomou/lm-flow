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

#include "lmflow/flow.hpp"

/* 只在 64 位平台断言具体数值;其它平台仅校验相对关系 */
#if UINTPTR_MAX == 0xFFFFFFFFFFFFFFFFu

static_assert(sizeof(void*) == 8, "expected 64-bit pointer");

/* LMFlowPacket: payload | type_id | timestamp | owner | drop_fn */
static_assert(sizeof(LMFlowPacket) == 40, "LMFlowPacket size changed — sync tests/abi_layout.rs");
static_assert(alignof(LMFlowPacket) == 8, "LMFlowPacket alignment changed");
static_assert(offsetof(LMFlowPacket, payload) == 0, "LMFlowPacket::payload offset changed");
static_assert(offsetof(LMFlowPacket, type_id) == 8, "LMFlowPacket::type_id offset changed");
static_assert(offsetof(LMFlowPacket, timestamp) == 16, "LMFlowPacket::timestamp offset changed");
static_assert(offsetof(LMFlowPacket, owner) == 24, "LMFlowPacket::owner offset changed");
static_assert(offsetof(LMFlowPacket, drop_fn) == 32, "LMFlowPacket::drop_fn offset changed");

/* LMFlowKernelVTable: 6 个函数指针 */
static_assert(sizeof(LMFlowKernelVTable) == 48, "LMFlowKernelVTable size changed");
static_assert(alignof(LMFlowKernelVTable) == 8, "LMFlowKernelVTable alignment changed");
static_assert(offsetof(LMFlowKernelVTable, create) == 0, "vtable::create offset changed");
static_assert(offsetof(LMFlowKernelVTable, get_contract) == 8, "vtable::get_contract offset changed");
static_assert(offsetof(LMFlowKernelVTable, open) == 16, "vtable::open offset changed");
static_assert(offsetof(LMFlowKernelVTable, process) == 24, "vtable::process offset changed");
static_assert(offsetof(LMFlowKernelVTable, close) == 32, "vtable::close offset changed");
static_assert(offsetof(LMFlowKernelVTable, destroy) == 40, "vtable::destroy offset changed");

/* LMFlowBuffer: data | shape[8] | strides[8] | ndim | dtype | flags | device | reserved[2] */
static_assert(sizeof(LMFlowBuffer) == 8 + 64 + 64 + 4 + 4 + 4 + 4 + 16,
              "LMFlowBuffer size changed — sync Rust side and bump LMFLOW_ABI_VERSION");
static_assert(alignof(LMFlowBuffer) == 8, "LMFlowBuffer alignment changed");
static_assert(offsetof(LMFlowBuffer, data) == 0, "LMFlowBuffer::data offset changed");
static_assert(offsetof(LMFlowBuffer, shape) == 8, "LMFlowBuffer::shape offset changed");
static_assert(offsetof(LMFlowBuffer, strides) == 72, "LMFlowBuffer::strides offset changed");
static_assert(offsetof(LMFlowBuffer, ndim) == 136, "LMFlowBuffer::ndim offset changed");
static_assert(offsetof(LMFlowBuffer, dtype) == 140, "LMFlowBuffer::dtype offset changed");
static_assert(offsetof(LMFlowBuffer, flags) == 144, "LMFlowBuffer::flags offset changed");
static_assert(offsetof(LMFlowBuffer, device) == 148, "LMFlowBuffer::device offset changed");
static_assert(offsetof(LMFlowBuffer, reserved) == 152, "LMFlowBuffer::reserved offset changed");
static_assert(LMFLOW_MAX_DIMS == 8, "changing LMFLOW_MAX_DIMS alters LMFlowBuffer layout");

static_assert(offsetof(LMFlowInputQueueStats, struct_size) == 0,
              "LMFlowInputQueueStats::struct_size must remain first");
#endif /* 64-bit */

/* 状态码 / 时间戳哨兵的取值约定 */
static_assert(LMFLOW_OK == 0, "LMFLOW_OK must be 0");
static_assert(LMFLOW_TS_UNSET < LMFLOW_TS_UNSTARTED, "timestamp sentinel ordering is wrong");
static_assert(LMFLOW_TS_UNSTARTED < LMFLOW_TS_PRE_STREAM, "timestamp sentinel ordering is wrong");
static_assert(LMFLOW_TS_PRE_STREAM < LMFLOW_TS_MIN, "timestamp sentinel ordering is wrong");
static_assert(LMFLOW_TS_MIN < LMFLOW_TS_MAX, "timestamp sentinel ordering is wrong");
static_assert(LMFLOW_TS_MAX < LMFLOW_TS_POST_STREAM, "timestamp sentinel ordering is wrong");
static_assert(LMFLOW_TS_POST_STREAM < LMFLOW_TS_ONE_OVER_POST_STREAM, "timestamp sentinel ordering is wrong");
static_assert(LMFLOW_TS_ONE_OVER_POST_STREAM < LMFLOW_TS_DONE, "timestamp sentinel ordering is wrong");

/* ---- type_id 的哈希算法:C++ 与 Rust 各有一份独立实现,必须同结果 ----
 *
 * ADR #22:`type_id` = FNV-1a(修饰名),并把 0..15 让给内建类型。C++ 侧在
 * `flow.hpp` 的 `Fnv1a` + `NormalizeTypeId`,Rust 侧在 `packet.rs` 的
 * `fnv1a_type_id` —— **两份各自手写的实现**。任何一侧动了常量、乘子或
 * 那个 `< 16` 的规避分支,跨语言的类型校验就会静默失配(同一个类型在两边
 * 算出不同 id,契约检查形同虚设)。
 *
 * 故把同一个字面量在**两侧**钉死:这里编译期 `static_assert`,Rust 侧
 * `tests/abi_layout.rs` 运行期 `assert_eq!`,取的是同一个数。
 * 期望值由第三方实现(Python)独立算出,不是从任一侧抄来的。 */
static_assert(lmflow::Fnv1a("") == 14695981039346656037ULL,
              "FNV-1a 64 位 offset basis 变了 —— 与 Rust 侧 fnv1a_type_id 不再一致");
static_assert(lmflow::NormalizeTypeId(lmflow::Fnv1a("lmflow.test.Stable")) ==
                  0xBFB531B283179309ULL,
              "type_id 哈希结果变了 —— 必须与 tests/abi_layout.rs 的同名常量同步");

/* 内建区规避:0..15 留给内建类型,自定义标识不得落进去。 */
static_assert(lmflow::NormalizeTypeId(0) == 16, "NormalizeTypeId must lift 0 out of the builtin range");
static_assert(lmflow::NormalizeTypeId(15) == 31, "NormalizeTypeId must lift 15 out of the builtin range");
static_assert(lmflow::NormalizeTypeId(16) == 16, "NormalizeTypeId must leave 16 alone");
static_assert(lmflow::NormalizeTypeId(17) == 17, "NormalizeTypeId must leave 17 alone");

/* ---- 保持糖层宏路径处于编译状态(不参与运行,仅编译期覆盖)---- */
namespace {
class AbiProbeKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) { c.InputSetAny(0); }
  lmflow::Status Process(lmflow::Context&) override { return lmflow::Status::Ok(); }
};
}  // namespace

LMFLOW_REGISTER_KERNEL(AbiProbeKernel)
