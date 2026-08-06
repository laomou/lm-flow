//! C ABI:算子注册与 Contract(端口类型契约)
//!
//! 由 [`super`](../index.html) 的分节拆出 —— 见 `ffi/mod.rs` 的模块头注释,
//! 那里定义了整层的约定(catch_unwind 包裹、last_error、空指针检查、布局钉死)。
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void};

use crate::kernel::{KernelLanguage, KernelVTable};
use crate::runtime::{self};
use crate::status::Error;

use super::*;

// ---------------------------------------------------------------- 算子注册

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LMFlowKernelVTable {
    pub create: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    pub get_contract: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContract)>,
    pub open: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
    pub process: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
    pub close: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_register_kernel(
    name: *const c_char,
    vt: *const LMFlowKernelVTable,
    factory: *mut c_void,
) -> i32 {
    register_kernel_impl(name, vt, factory, KernelLanguage::Unknown)
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_register_kernel_with_language(
    name: *const c_char,
    vt: *const LMFlowKernelVTable,
    factory: *mut c_void,
    language: u32,
) -> i32 {
    let language = match language {
        0 => KernelLanguage::Unknown,
        1 => KernelLanguage::Rust,
        2 => KernelLanguage::Cpp,
        3 => KernelLanguage::Python,
        4 => KernelLanguage::C,
        other => {
            return guard(|| {
                fail(Error::InvalidArg(format!(
                    "unknown kernel implementation language id {other}"
                )))
            })
        }
    };
    register_kernel_impl(name, vt, factory, language)
}

unsafe fn register_kernel_impl(
    name: *const c_char,
    vt: *const LMFlowKernelVTable,
    factory: *mut c_void,
    language: KernelLanguage,
) -> i32 {
    guard(|| {
        let Some(n) = cstr(name) else {
            return fail(Error::InvalidArg(
                "kernel name is empty or not UTF-8".into(),
            ));
        };
        if vt.is_null() {
            return fail(Error::InvalidArg(format!("kernel `{n}` vtable is null")));
        }
        let v = &*vt;
        // LMFlowKernelVTable 与 KernelVTable 布局相同,只是 ctx 参数的具体类型不同
        // LMFlowKernelVTable 与 KernelVTable 的函数指针 ABI 完全相同,仅上下文参数的
        // 具名类型不同(LMFlowContext*/LMFlowContract* ↔ void*)。显式标注转换目标类型。
        type CtxFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
        type ContractFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
        let converted = KernelVTable {
            create: v.create,
            get_contract: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContract)>,
                Option<ContractFn>,
            >(v.get_contract),
            open: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
                Option<CtxFn>,
            >(v.open),
            process: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
                Option<CtxFn>,
            >(v.process),
            close: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
                Option<CtxFn>,
            >(v.close),
            destroy: v.destroy,
        };
        to_status(crate::kernel::register_with_language(
            n, converted, factory, language,
        ))
    })
}

#[no_mangle]
pub extern "C" fn lmflow_registered_kernel_count() -> usize {
    guard_val(0, || crate::kernel::registered_names().len())
}

#[no_mangle]
pub extern "C" fn lmflow_registered_kernel_name(idx: usize) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        static ARENA: std::sync::LazyLock<runtime::CStrArena> =
            std::sync::LazyLock::new(runtime::CStrArena::default);
        match crate::kernel::registered_names().get(idx) {
            Some(n) => ARENA.intern(n),
            None => c"".as_ptr(),
        }
    })
}

// ---------------------------------------------------------------- Contract

#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_num_inputs(c: *const LMFlowContract) -> usize {
    guard_val(0, || {
        contract_mut(c as *mut LMFlowContract).map_or(0, |x| x.inputs.len())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_num_outputs(c: *const LMFlowContract) -> usize {
    guard_val(0, || {
        contract_mut(c as *mut LMFlowContract).map_or(0, |x| x.outputs.len())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_id(
    c: *const LMFlowContract,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.inputs
            .id_by_tag(cstr(tag).unwrap_or(""), index)
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_id(
    c: *const LMFlowContract,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.outputs
            .id_by_tag(cstr(tag).unwrap_or(""), index)
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_index(
    c: *const LMFlowContract,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.inputs
            .index_by_name(cstr(name).unwrap_or(""))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_index(
    c: *const LMFlowContract,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.outputs
            .index_by_name(cstr(name).unwrap_or(""))
            .unwrap_or(INVALID_ID)
    })
}

/// contract 阶段的端口名指针需要一个稳定住处;用进程级 arena。
fn contract_arena() -> &'static runtime::CStrArena {
    static A: std::sync::LazyLock<runtime::CStrArena> =
        std::sync::LazyLock::new(runtime::CStrArena::default);
    &A
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_name(
    c: *const LMFlowContract,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        contract_mut(c as *mut LMFlowContract)
            .and_then(|x| x.inputs.name(idx).map(|s| contract_arena().intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_name(
    c: *const LMFlowContract,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        contract_mut(c as *mut LMFlowContract)
            .and_then(|x| x.outputs.name(idx).map(|s| contract_arena().intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_set_any(c: *mut LMFlowContract, idx: usize) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.input_types.get_mut(idx) {
                *s = 0;
            } else {
                x.record_error(format!(
                    "input port index {idx} is out of range (num_inputs={})",
                    x.input_types.len()
                ));
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_set_any(c: *mut LMFlowContract, idx: usize) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.output_types.get_mut(idx) {
                *s = 0;
            } else {
                x.record_error(format!(
                    "output port index {idx} is out of range (num_outputs={})",
                    x.output_types.len()
                ));
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_set_type(
    c: *mut LMFlowContract,
    idx: usize,
    type_id: u64,
) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.input_types.get_mut(idx) {
                *s = type_id;
            } else {
                x.record_error(format!(
                    "input port index {idx} is out of range (num_inputs={})",
                    x.input_types.len()
                ));
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_set_type(
    c: *mut LMFlowContract,
    idx: usize,
    type_id: u64,
) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.output_types.get_mut(idx) {
                *s = type_id;
            } else {
                x.record_error(format!(
                    "output port index {idx} is out of range (num_outputs={})",
                    x.output_types.len()
                ));
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_require_side_packet(
    c: *mut LMFlowContract,
    name: *const c_char,
) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(name) = cstr(name).filter(|name| !name.is_empty()) {
                x.required_side_packets.push(name.to_string());
            } else {
                x.record_error("required side packet name must not be empty or invalid UTF-8");
            }
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_set_error(c: *mut LMFlowContract, message: *const c_char) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            let message = cstr(message)
                .filter(|message| !message.is_empty())
                .unwrap_or("GetContract failed without a valid UTF-8 error message");
            x.record_error(message);
        }
    });
}
