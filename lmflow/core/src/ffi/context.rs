//! C ABI:Context —— 算子回调期的读输入 / 发输出 / options / side packet
//!
//! 由 [`super`](../index.html) 的分节拆出 —— 见 `ffi/mod.rs` 的模块头注释,
//! 那里定义了整层的约定(catch_unwind 包裹、last_error、空指针检查、布局钉死)。
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void};

use crate::runtime::last_error;
use crate::status::{code, Error};
use crate::timestamp::Timestamp;

use super::*;

// ---------------------------------------------------------------- Context

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_num_inputs(c: *const LMFlowContext) -> usize {
    guard_val(0, || ctx_ref(c).map_or(0, |x| x.in_ports.len()))
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_num_outputs(c: *const LMFlowContext) -> usize {
    guard_val(0, || ctx_ref(c).map_or(0, |x| x.out_ports.len()))
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_id(
    c: *const LMFlowContext,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.in_ports.id_by_tag(cstr(tag).unwrap_or(""), index))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_output_id(
    c: *const LMFlowContext,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.out_ports.id_by_tag(cstr(tag).unwrap_or(""), index))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_index(
    c: *const LMFlowContext,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.in_ports.index_by_name(cstr(name).unwrap_or("")))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_output_index(
    c: *const LMFlowContext,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.out_ports.index_by_name(cstr(name).unwrap_or("")))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_name(
    c: *const LMFlowContext,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c)
            .and_then(|x| x.in_ports.name(idx).map(|s| x.intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_output_name(
    c: *const LMFlowContext,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c)
            .and_then(|x| x.out_ports.name(idx).map(|s| x.intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_node_name(c: *const LMFlowContext) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c).map_or(c"".as_ptr(), |x| x.node_name_cstr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_kernel_name(c: *const LMFlowContext) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c).map_or(c"".as_ptr(), |x| x.kernel_name_cstr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_log(c: *const LMFlowContext, level: i32, msg: *const c_char) {
    guard_val((), || {
        if let (Some(x), Some(m)) = (ctx_ref(c), cstr(msg)) {
            x.log(level, m);
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_set_error(c: *const LMFlowContext, msg: *const c_char) {
    guard_val((), || {
        // set_error 需要可变访问;上下文在回调期间由算子独占,故此转换安全
        if let (Some(x), Some(m)) = (ctx_mut(c as *mut LMFlowContext), cstr(msg)) {
            x.set_error(m);
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_source_done(c: *const LMFlowContext) {
    guard_val((), || {
        // 源算子自报「已产完」;上下文在回调期间由算子独占,可变转换安全。
        if let Some(x) = ctx_mut(c as *mut LMFlowContext) {
            x.source_done = true;
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_source_yield(c: *const LMFlowContext, delay_ms: u64) {
    guard_val((), || {
        if let Some(x) = ctx_mut(c as *mut LMFlowContext) {
            x.source_yield = Some(std::time::Duration::from_millis(delay_ms));
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_close_reason(c: *const LMFlowContext) -> i32 {
    guard_val(0, || ctx_ref(c).map_or(0, |x| x.close_reason))
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_counter_add(
    c: *const LMFlowContext,
    name: *const c_char,
    delta: i64,
) {
    guard_val((), || {
        if let (Some(x), Some(n)) = (ctx_ref(c), cstr(name)) {
            x.shared.counter_add(n, delta);
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_is_empty(c: *const LMFlowContext, idx: usize) -> bool {
    guard_val(true, || {
        ctx_ref(c).is_none_or(|x| x.input(idx).is_none_or(|p| p.is_empty()))
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_is_done(c: *const LMFlowContext, idx: usize) -> bool {
    guard_val(false, || {
        ctx_ref(c).is_some_and(|x| x.inputs_done.get(idx).copied().unwrap_or(false))
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input(c: *const LMFlowContext, idx: usize) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || match ctx_ref(c) {
        Some(x) => x.input(idx).map(borrow_packet).unwrap_or_default(),
        None => LMFlowPacket::default(),
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_count(c: *const LMFlowContext, idx: usize) -> usize {
    // 本次调用某输入口的包数:单包策略恒 0/1;batch 策略为该批实际大小。
    guard_val(0, || ctx_ref(c).map_or(0, |x| x.input_count(idx)))
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_at(
    c: *const LMFlowContext,
    idx: usize,
    k: usize,
) -> LMFlowPacket {
    // 借用第 k 个输入包(语义同 lmflow_ctx_input,不转移引用计数)。batch 策略下遍历一批。
    guard_val(LMFlowPacket::default(), || match ctx_ref(c) {
        Some(x) => x.input_at(idx, k).map(borrow_packet).unwrap_or_default(),
        None => LMFlowPacket::default(),
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_payload(
    c: *const LMFlowContext,
    idx: usize,
) -> *const c_void {
    guard_val(std::ptr::null(), || {
        ctx_ref(c)
            .and_then(|x| x.input(idx))
            .and_then(|p| p.arc_ref().map(|a| a.payload.data_ptr() as *const c_void))
            .unwrap_or(std::ptr::null())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_timestamp(c: *const LMFlowContext) -> i64 {
    guard_val(Timestamp::unset().0, || {
        ctx_ref(c).map_or(Timestamp::unset().0, |x| x.input_ts.0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_take_input(c: *mut LMFlowContext, idx: usize) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || match ctx_mut(c) {
        Some(x) => own_packet(x.take_input(idx)),
        None => LMFlowPacket::default(),
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_emit(c: *mut LMFlowContext, out_idx: usize, pkt: LMFlowPacket) {
    guard_val((), || {
        let p = take_packet(pkt);
        if let Some(x) = ctx_mut(c) {
            if let Err(e) = x.emit(out_idx, p) {
                x.set_error(&e.to_string());
                last_error::set(&e.to_string());
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_forward(c: *mut LMFlowContext, in_idx: usize, out_idx: usize) {
    guard_val((), || {
        if let Some(x) = ctx_mut(c) {
            if let Err(e) = x.forward(in_idx, out_idx) {
                x.set_error(&e.to_string());
                last_error::set(&e.to_string());
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_set_next_ts_bound(
    c: *mut LMFlowContext,
    out_idx: usize,
    bound: i64,
) {
    guard_val((), || {
        if let Some(x) = ctx_mut(c) {
            x.set_next_bound(out_idx, Timestamp(bound));
        }
    });
}

// ---- options ----

macro_rules! opt_scalar {
    ($name:ident, $method:ident, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            c: *const LMFlowContext,
            key: *const c_char,
            def: $ty,
        ) -> $ty {
            guard_val(def, || {
                ctx_ref(c)
                    .and_then(|x| cstr(key).and_then(|k| x.options.$method(k)))
                    .unwrap_or(def)
            })
        }
    };
}
opt_scalar!(lmflow_ctx_option_i64, i64, i64);
opt_scalar!(lmflow_ctx_option_f64, f64, f64);
opt_scalar!(lmflow_ctx_option_bool, bool, bool);

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_has_option(
    c: *const LMFlowContext,
    key: *const c_char,
) -> bool {
    guard_val(false, || {
        ctx_ref(c).is_some_and(|x| cstr(key).is_some_and(|k| x.options.has(k)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_str(
    c: *const LMFlowContext,
    key: *const c_char,
    def: *const c_char,
) -> *const c_char {
    guard_val(def, || {
        ctx_ref(c)
            .and_then(|x| cstr(key).and_then(|k| x.options.str_cstr(k)))
            .unwrap_or(def)
    })
}

macro_rules! require_scalar {
    ($name:ident, $method:ident, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            c: *const LMFlowContext,
            key: *const c_char,
            out: *mut $ty,
        ) -> i32 {
            guard(|| {
                let Some(x) = ctx_ref(c) else {
                    return fail(Error::InvalidArg("context is null".into()));
                };
                let Some(k) = cstr(key) else {
                    return fail(Error::InvalidArg("parameter name is null".into()));
                };
                match x.options.$method(k) {
                    Some(v) => {
                        if !out.is_null() {
                            *out = v;
                        }
                        code::OK
                    }
                    None => fail(Error::InvalidArg(format!(
                        "node `{}`: required parameter options.{k} is missing or type mismatch",
                        x.node_name
                    ))),
                }
            })
        }
    };
}
require_scalar!(lmflow_ctx_require_option_i64, i64, i64);
require_scalar!(lmflow_ctx_require_option_f64, f64, f64);
require_scalar!(lmflow_ctx_require_option_bool, bool, bool);

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_require_option_str(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut *const c_char,
) -> i32 {
    guard(|| {
        let Some(x) = ctx_ref(c) else {
            return fail(Error::InvalidArg("context is null".into()));
        };
        let Some(k) = cstr(key) else {
            return fail(Error::InvalidArg("parameter name is null".into()));
        };
        match x.options.str_cstr(k) {
            Some(p) => {
                if !out.is_null() {
                    *out = p;
                }
                code::OK
            }
            None => fail(Error::InvalidArg(format!(
                "node `{}`: required parameter options.{k} is missing or type mismatch",
                x.node_name
            ))),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_count(
    c: *const LMFlowContext,
    key: *const c_char,
) -> usize {
    guard_val(0, || {
        ctx_ref(c).map_or(0, |x| cstr(key).map_or(0, |k| x.options.count(k)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_i64_array(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut i64,
    cap: usize,
) -> usize {
    guard_val(0, || {
        let (Some(x), Some(k)) = (ctx_ref(c), cstr(key)) else {
            return 0;
        };
        let mut tmp = vec![0i64; cap];
        let n = x.options.i64_array(k, &mut tmp);
        if !out.is_null() && cap > 0 {
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), out, cap.min(n));
        }
        n
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_f64_array(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut f64,
    cap: usize,
) -> usize {
    guard_val(0, || {
        let (Some(x), Some(k)) = (ctx_ref(c), cstr(key)) else {
            return 0;
        };
        let mut tmp = vec![0f64; cap];
        let n = x.options.f64_array(k, &mut tmp);
        if !out.is_null() && cap > 0 {
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), out, cap.min(n));
        }
        n
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_str_array(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut *const c_char,
    cap: usize,
) -> usize {
    guard_val(0, || {
        let (Some(x), Some(k)) = (ctx_ref(c), cstr(key)) else {
            return 0;
        };
        let mut tmp = vec![std::ptr::null(); cap];
        let n = x.options.str_array(k, &mut tmp);
        if !out.is_null() && cap > 0 {
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), out, cap.min(n));
        }
        n
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_options_json(c: *const LMFlowContext) -> *const c_char {
    guard_val(c"{}".as_ptr(), || {
        ctx_ref(c).map_or(c"{}".as_ptr(), |x| x.options.json_cstr())
    })
}

// ---- side packet ----

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_has_side_packet(
    c: *const LMFlowContext,
    name: *const c_char,
) -> bool {
    guard_val(false, || {
        ctx_ref(c).is_some_and(|x| cstr(name).is_some_and(|n| x.side_packets.contains_key(n)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_side_packet(
    c: *const LMFlowContext,
    name: *const c_char,
) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        ctx_ref(c)
            .and_then(|x| cstr(name).and_then(|n| x.side_packets.get(n)))
            .map(borrow_packet)
            .unwrap_or_default()
    })
}
