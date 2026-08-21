//! The C ABI layer: the implementation behind `include/lmflow/flow.h`.
//!
//! Conventions:
//!  * Every exported function is wrapped in `catch_unwind` — letting a Rust panic cross an FFI
//!    boundary is undefined behaviour.
//!  * On failure the thread-local `lmflow_last_error` is set, so the caller can obtain a readable
//!    reason.
//!  * Structs that cross the boundary are `#[repr(C)]`, with the layout pinned from both sides by
//!    `tests/abi_layout.rs` and `cpp/abi_assert.cc`.
//!  * Every exported function null-checks its pointers and returns an error code or a default
//!    value rather than crashing.
//!
//! On `missing_safety_doc`: the functions here are consumed by **C callers**, whose safety
//! contract — pointer validity, ownership transfer, lifetimes — is authoritatively defined by the
//! comments in `include/lmflow/flow.h`, which is the documentation C and C++ users actually read.
//! Restating it here in Rust `# Safety` form would only create two descriptions that drift apart,
//! so the lint is waived for the whole module.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::context::Context;
use crate::graph::GraphInner;
use crate::kernel::Contract;
use crate::kernel_runner::KernelRunner;
use crate::runtime::last_error;
use crate::status::{code, Error};

pub const ABI_VERSION: u32 = 4;

pub const INVALID_ID: usize = usize::MAX;
pub const LMFLOW_POLLER_BLOCK: i32 = 0;
pub const LMFLOW_POLLER_DROP_OLDEST: i32 = 1;
pub const LMFLOW_POLLER_DROP_NEWEST: i32 = 2;
pub const LMFLOW_POLLER_LATEST: i32 = 3;

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
pub struct LMFlowKernelRunner {
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

/// The object behind a graph input-port handle; its lifetime follows the graph.
pub struct InputHandle {
    graph: Arc<GraphInner>,
    edge: usize,
}

pub struct KernelRunnerHandle {
    pub runner: KernelRunner,
}

// 按 API 域拆成子模块;`pub use` 把条目原样再导出,故 `lmflow::ffi::X` 路径不变。
mod context;
mod graph;
mod kernel;
mod kernel_runner;
mod packet;

pub use context::*;
pub use graph::*;
pub use kernel::*;
pub use kernel_runner::*;
pub use packet::*;
