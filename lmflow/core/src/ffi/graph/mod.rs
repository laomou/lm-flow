//! C ABI:图:生命周期、输入口、poller/observer、内省
//!
//! 由 [`super`](../index.html) 的分节拆出 —— 见 `ffi/mod.rs` 的模块头注释,
//! 那里定义了整层的约定(catch_unwind 包裹、last_error、空指针检查、布局钉死)。
#![allow(clippy::missing_safety_doc)]

use std::ffi::c_void;

use crate::graph::{Graph, Poller, State};
use crate::runtime;
use crate::status::Error;

use super::*;

mod diagnostics;
mod input_poller;
mod lifecycle;
mod observer;

pub use diagnostics::*;
pub use input_poller::*;
pub use lifecycle::*;
pub use observer::*;

/// `lmflow_graph_new` 先返回一个空槽,`init_from_yaml` 才真正建图。
///
/// 输入/输出句柄(`LMFlowInput*`/`LMFlowPoller*`)**不**由本槽持有 —— 它们是**调用方拥有**的:
/// `lmflow_graph_input`/`lmflow_graph_add_poller` 返回一个独立的 `Box::into_raw` 句柄,
/// 各自持一份 `Arc<GraphInner>`,须由调用方 `lmflow_input_free`/`lmflow_poller_free` 释放。
/// 这样即使先 `lmflow_graph_free` 了图,句柄内存依旧有效(其 Arc 撑着引擎),
/// 之后再用只会得到「图已结束」的错误,而不是 use-after-free。
#[derive(Default)]
pub struct GraphSlot {
    graph: Option<Graph>,
}

unsafe fn slot_mut<'a>(g: *mut LMFlowGraph) -> Option<&'a mut GraphSlot> {
    if g.is_null() {
        None
    } else {
        Some(&mut *(g as *mut GraphSlot))
    }
}

unsafe fn with_graph<F: FnOnce(&Graph) -> i32>(g: *mut LMFlowGraph, f: F) -> i32 {
    match slot_mut(g).and_then(|s| s.graph.as_ref()) {
        Some(gr) => f(gr),
        None => fail(Error::State("graph not yet initialized".into())),
    }
}

// Shared graph handle lookup used by submodules.
pub(super) unsafe fn graph_of<'a>(g: *mut LMFlowGraph) -> Option<&'a Graph> {
    slot_mut(g).and_then(|s| s.graph.as_ref())
}
