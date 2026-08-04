//! 跨界结构体的布局一致性测试。
//!
//! 与 `cpp/abi_assert.cc` 的 `static_assert` **钉在同一组常量上**:任一侧改了字段
//! 而忘了同步另一侧,构建/测试就会失败,而不是留到运行期内存错乱。
//!
//! 这些数字不是"从代码里读出来的",而是**契约**。改它们必须同时改:
//!   1. `include/flow.h` 的结构体定义
//!   2. `cpp/abi_assert.cc` 的 static_assert
//!   3. 本文件
//!   4. 提升 `LMFLOW_ABI_VERSION`

use std::mem::{align_of, offset_of, size_of};

use lmflow::ffi::{LMFlowBuffer, LMFlowInputQueueStats, LMFlowNodeStats, LMFlowPacket};

#[test]
fn lmflow_packet_layout() {
    assert_eq!(size_of::<LMFlowPacket>(), 40, "LMFlowPacket size");
    assert_eq!(align_of::<LMFlowPacket>(), 8, "LMFlowPacket alignment");
    assert_eq!(offset_of!(LMFlowPacket, payload), 0);
    assert_eq!(offset_of!(LMFlowPacket, type_id), 8);
    assert_eq!(offset_of!(LMFlowPacket, timestamp), 16);
    assert_eq!(offset_of!(LMFlowPacket, owner), 24);
    assert_eq!(offset_of!(LMFlowPacket, drop_fn), 32);
}

#[test]
fn flow_buffer_layout() {
    // data(8) + shape[8](64) + strides[8](64) + ndim(4) + dtype(4) + flags(4) + device(4) + reserved[2](16)
    assert_eq!(size_of::<LMFlowBuffer>(), 168, "LMFlowBuffer size");
    assert_eq!(align_of::<LMFlowBuffer>(), 8, "LMFlowBuffer alignment");
    assert_eq!(offset_of!(LMFlowBuffer, data), 0);
    assert_eq!(offset_of!(LMFlowBuffer, shape), 8);
    assert_eq!(offset_of!(LMFlowBuffer, strides), 72);
    assert_eq!(offset_of!(LMFlowBuffer, ndim), 136);
    assert_eq!(offset_of!(LMFlowBuffer, dtype), 140);
    assert_eq!(offset_of!(LMFlowBuffer, flags), 144);
    assert_eq!(offset_of!(LMFlowBuffer, device), 148);
    assert_eq!(offset_of!(LMFlowBuffer, reserved), 152);
}

#[test]
fn flow_node_stats_uses_struct_size_for_forward_compat() {
    // 与 LMFlowBuffer 不同,统计结构体用入参 struct_size 做前向兼容(见 docs/design.md §15.1),
    // 所以这里只钉住「struct_size 是第一个字段」这一契约,而不钉总大小。
    assert_eq!(offset_of!(LMFlowNodeStats, struct_size), 0);
    assert_eq!(size_of::<u32>(), 4);
}

#[test]
fn input_queue_stats_uses_struct_size_for_forward_compat() {
    assert_eq!(offset_of!(LMFlowInputQueueStats, struct_size), 0);
    assert_eq!(size_of::<u32>(), 4);
}

#[test]
fn abi_version_matches_header() {
    // include/flow.h: #define LMFLOW_ABI_VERSION 2u
    assert_eq!(lmflow::ffi::lmflow_abi_version(), 2);
}

#[test]
fn invalid_id_matches_header() {
    // include/flow.h: #define LMFLOW_INVALID_ID ((size_t)-1)
    assert_eq!(lmflow::ffi::INVALID_ID, usize::MAX);
}

#[test]
fn status_codes_match_header() {
    use lmflow::status::code;
    assert_eq!(code::OK, 0);
    assert_eq!(code::INVALID_ARG, 1);
    assert_eq!(code::NOT_FOUND, 2);
    assert_eq!(code::KERNEL, 3);
    assert_eq!(code::PANIC, 4);
    assert_eq!(code::WOULD_BLOCK, 5);
    assert_eq!(code::TIMEOUT, 6);
    assert_eq!(code::CANCELLED, 7);
    assert_eq!(code::CLOSED, 8);
    assert_eq!(code::ABI, 9);
    assert_eq!(code::UNSUPPORTED, 10);
    assert_eq!(code::STATE, 11);
}

#[test]
fn poller_overflow_policy_ids_match_header() {
    use lmflow::ffi::{
        LMFLOW_POLLER_BLOCK, LMFLOW_POLLER_DROP_NEWEST, LMFLOW_POLLER_DROP_OLDEST,
        LMFLOW_POLLER_LATEST,
    };
    assert_eq!(LMFLOW_POLLER_BLOCK, 0);
    assert_eq!(LMFLOW_POLLER_DROP_OLDEST, 1);
    assert_eq!(LMFLOW_POLLER_DROP_NEWEST, 2);
    assert_eq!(LMFLOW_POLLER_LATEST, 3);
}

#[test]
fn dot_view_ids_match_header() {
    use lmflow::ffi::{LMFLOW_DOT_COMPACT, LMFLOW_DOT_DIAGNOSTICS, LMFLOW_DOT_TOPOLOGY};
    assert_eq!(LMFLOW_DOT_TOPOLOGY, 0);
    assert_eq!(LMFLOW_DOT_COMPACT, 1);
    assert_eq!(LMFLOW_DOT_DIAGNOSTICS, 2);
}

#[test]
fn builtin_type_ids_match_header() {
    use lmflow::packet::type_id;
    assert_eq!(type_id::NONE, 0);
    assert_eq!(type_id::BYTES, 1);
    assert_eq!(type_id::I64, 2);
    assert_eq!(type_id::F64, 3);
    assert_eq!(type_id::BOOL, 4);
    assert_eq!(type_id::STR, 5);
    assert_eq!(type_id::BUFFER, 6);
    assert_eq!(type_id::HOST_OBJECT, 7);
}

/// `type_id` 的哈希算法:C++ 与 Rust **各有一份独立手写实现**,必须同结果。
///
/// ADR #22:`type_id` = FNV-1a(修饰名),0..15 让给内建类型。C++ 在 `flow.hpp` 的
/// `Fnv1a` + `NormalizeTypeId`,Rust 在 `packet::fnv1a_type_id`。任何一侧动了 offset
/// basis、乘子、或那个 `< 16` 的规避分支,跨语言类型校验就会**静默失配** —— 同一个类型
/// 在两边算出不同 id,契约检查形同虚设,而且不会有任何报错。
///
/// 故把同一个字面量在两侧钉死:这里运行期断言,`cpp/abi_assert.cc` 编译期 `static_assert`,
/// 取的是同一个数。期望值由第三方实现(Python)独立算出,不是从任一侧抄来的。
#[test]
fn type_id_hash_matches_cpp_side() {
    use lmflow::packet::fnv1a_type_id;

    // 与 cpp/abi_assert.cc 的 static_assert 同一个常量。
    assert_eq!(
        fnv1a_type_id("lmflow.test.Stable"),
        0xBFB5_31B2_8317_9309,
        "Rust 侧 FNV-1a 与 C++ 侧不再一致 —— 跨语言 type_id 会静默失配"
    );

    // 修饰名示例:Itanium ABI 下 `int` 的 typeid().name() 是 "i"、`double` 是 "d"。
    // 这两条钉的是「同一个字符串两侧必得同一个数」,故值同样由 Python 独立算出。
    assert_eq!(fnv1a_type_id("i"), 0xAF63_E44C_8601_FA24);
    assert_eq!(fnv1a_type_id("d"), 0xAF63_D94C_8601_E773);

    // 内建区(0..15)必须被规避:自定义标识永不落进去。
    // 直接构造小哈希不可行(64 位 FNV 撞进 0..15 的概率约 1e-18),故改为
    // 大量取样都 >= 16 —— 配合 C++ 侧对 NormalizeTypeId(0/15/16/17) 的编译期断言,
    // 两处合起来覆盖了这条规则的边界与实效。
    for i in 0..2000 {
        assert!(
            fnv1a_type_id(&format!("lmflow.sample.{i}")) >= 16,
            "自定义 type_id 不得落入内建区 0..15"
        );
    }
}

#[test]
fn dtype_ids_and_sizes_match_header() {
    use lmflow::packet::{dtype, dtype_size};
    assert_eq!(dtype::U8, 0);
    assert_eq!(dtype::I8, 1);
    assert_eq!(dtype::U16, 2);
    assert_eq!(dtype::I16, 3);
    assert_eq!(dtype::I32, 4);
    assert_eq!(dtype::I64, 5);
    assert_eq!(dtype::F16, 6);
    assert_eq!(dtype::F32, 7);
    assert_eq!(dtype::F64, 8);
    assert_eq!(dtype_size(dtype::F32), 4);
}

#[test]
fn timestamp_sentinels_match_header() {
    use lmflow::Timestamp;
    assert_eq!(Timestamp::unset().0, i64::MIN);
    assert_eq!(Timestamp::unstarted().0, i64::MIN + 1);
    assert_eq!(Timestamp::pre_stream().0, i64::MIN + 2);
    assert_eq!(Timestamp::min().0, i64::MIN + 3);
    assert_eq!(Timestamp::max().0, i64::MAX - 3);
    assert_eq!(Timestamp::post_stream().0, i64::MAX - 2);
    assert_eq!(Timestamp::one_over_post_stream().0, i64::MAX - 1);
    assert_eq!(Timestamp::done().0, i64::MAX);
}

#[test]
fn max_dims_matches_header() {
    // include/flow.h: #define LMFLOW_MAX_DIMS 8 —— 改它会改变 LMFlowBuffer 布局
    assert_eq!(lmflow::packet::MAX_DIMS, 8);
}
