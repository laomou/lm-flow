//! C ABI 层:`include/flow.h` 的实现。
//!
//! 约定
//!  * 每个导出函数都用 `catch_unwind` 包裹 —— Rust panic 穿越 FFI 是 UB。
//!  * 失败时写线程局部 `lmflow_last_error`,让调用方能拿到可读原因。
//!  * 跨界结构体 `#[repr(C)]`,布局由 `tests/abi_layout.rs` 与 `cpp/abi_assert.cc` 双向钉死。
//!  * 所有导出函数对空指针都做检查,返回错误码/默认值而不是崩溃。
//!
//! 关于 `missing_safety_doc`:本模块的导出函数面向 **C 调用方**,其安全契约(指针有效性、
//! 所有权移交、生命周期)以 `include/flow.h` 的注释为权威定义 —— 那才是 C/C++ 用户会读的
//! 文档。在此重复一遍 Rust 风格的 `# Safety` 段落只会造成两处描述漂移,故整体豁免该 lint。
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::context::Context;
use crate::graph::GraphInner;
use crate::kernel::Contract;
use crate::runtime::last_error;
use crate::status::{code, Error};

pub const ABI_VERSION: u32 = 1;

pub const INVALID_ID: usize = usize::MAX;

// ---------------------------------------------------------------- 工具

fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            last_error::set("internal engine panic (caught at the FFI boundary)");
            code::PANIC
        }
    }
}

fn guard_val<T, F: FnOnce() -> T>(default: T, f: F) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            last_error::set("internal engine panic (caught at the FFI boundary)");
            default
        }
    }
}

fn fail(e: Error) -> i32 {
    last_error::set(&e.to_string());
    e.code()
}

fn to_status(r: crate::status::Result<()>) -> i32 {
    match r {
        Ok(()) => code::OK,
        Err(e) => fail(e),
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

unsafe fn ctx_ref<'a>(c: *const LMFlowContext) -> Option<&'a Context> {
    if c.is_null() {
        None
    } else {
        Some(&*(c as *const Context))
    }
}

unsafe fn ctx_mut<'a>(c: *mut LMFlowContext) -> Option<&'a mut Context> {
    if c.is_null() {
        None
    } else {
        Some(&mut *(c as *mut Context))
    }
}

unsafe fn contract_mut<'a>(c: *mut LMFlowContract) -> Option<&'a mut Contract> {
    if c.is_null() {
        None
    } else {
        Some(&mut *(c as *mut Contract))
    }
}

// ---------------------------------------------------------------- 不透明句柄

#[repr(C)]
pub struct LMFlowGraph {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowInput {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowPoller {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowContext {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowContract {
    _private: [u8; 0],
}

/// 图输入口句柄的实体(生命周期随 graph)。
pub struct InputHandle {
    graph: Arc<GraphInner>,
    edge: usize,
}

// 按 API 域拆成子模块;`pub use` 把条目原样再导出,故 `lmflow::ffi::X` 路径不变。
mod context;
mod graph;
mod kernel;
mod packet;

pub use context::*;
pub use graph::*;
pub use kernel::*;
pub use packet::*;
