//! C ABI:图:生命周期、输入口、poller/observer、内省
//!
//! 由 [`super`](../index.html) 的分节拆出 —— 见 `ffi/mod.rs` 的模块头注释,
//! 那里定义了整层的约定(catch_unwind 包裹、last_error、空指针检查、布局钉死)。
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void};

use crate::graph::{Graph, Poller, State};
use crate::runtime::{self, last_error};
use crate::status::{code, Error};

use super::*;

// ---------------------------------------------------------------- 图

#[no_mangle]
pub extern "C" fn lmflow_graph_new() -> *mut LMFlowGraph {
    guard_val(std::ptr::null_mut(), || {
        // 此处只分配空槽,真正建图在 init_from_yaml。
        // 顺便把错误状态清干净,免得调用方读到上一次遗留的 last_error。
        last_error::set("");
        Box::into_raw(Box::new(GraphSlot::default())) as *mut LMFlowGraph
    })
}

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

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_free(g: *mut LMFlowGraph) {
    guard_val((), || {
        if g.is_null() {
            return;
        }
        let slot = Box::from_raw(g as *mut GraphSlot);
        if let Some(gr) = &slot.graph {
            gr.cancel();
            let _ = gr.wait_done();
        }
        drop(slot);
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_init_from_yaml(
    g: *mut LMFlowGraph,
    yaml: *const c_char,
) -> i32 {
    guard(|| {
        let Some(slot) = slot_mut(g) else {
            return fail(Error::InvalidArg("graph handle is null".into()));
        };
        if slot.graph.is_some() {
            return fail(Error::State("graph already initialized".into()));
        }
        let Some(text) = cstr(yaml) else {
            return fail(Error::InvalidArg("yaml is empty or not UTF-8".into()));
        };
        match Graph::from_yaml(text) {
            Ok(gr) => {
                slot.graph = Some(gr);
                code::OK
            }
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_init_from_yaml_file(
    g: *mut LMFlowGraph,
    path: *const c_char,
) -> i32 {
    guard(|| {
        let Some(slot) = slot_mut(g) else {
            return fail(Error::InvalidArg("graph handle is null".into()));
        };
        let Some(p) = cstr(path) else {
            return fail(Error::InvalidArg("path is null".into()));
        };
        match Graph::from_yaml_file(p) {
            Ok(gr) => {
                slot.graph = Some(gr);
                code::OK
            }
            Err(e) => fail(e),
        }
    })
}

unsafe fn with_graph<F: FnOnce(&Graph) -> i32>(g: *mut LMFlowGraph, f: F) -> i32 {
    match slot_mut(g).and_then(|s| s.graph.as_ref()) {
        Some(gr) => f(gr),
        None => fail(Error::State("graph not yet initialized".into())),
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_start(g: *mut LMFlowGraph) -> i32 {
    guard(|| with_graph(g, |gr| to_status(gr.start())))
}

/// 复位已结束的图,使其可再次 `start`,保留已 open 的算子实例(见 flow.h)。
/// 图须处于 Terminated 且静止,否则返回 LMFLOW_ERR_STATE。
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_reset(g: *mut LMFlowGraph) -> i32 {
    guard(|| with_graph(g, |gr| to_status(gr.reset())))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_set_side_packet(
    g: *mut LMFlowGraph,
    name: *const c_char,
    pkt: LMFlowPacket,
) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        let Some(n) = cstr(name) else {
            return fail(Error::InvalidArg("side packet name is null".into()));
        };
        with_graph(g, |gr| to_status(gr.set_side_packet(n, p)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_input(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> *mut LMFlowInput {
    guard_val(std::ptr::null_mut(), || {
        let Some(slot) = slot_mut(g) else {
            last_error::set("graph handle is null");
            return std::ptr::null_mut();
        };
        let Some(gr) = slot.graph.as_ref() else {
            last_error::set("graph not yet initialized");
            return std::ptr::null_mut();
        };
        let Some(name) = cstr(port) else {
            last_error::set("port name is null");
            return std::ptr::null_mut();
        };
        let inner = gr.inner().clone();
        match inner.input_edge_by_name(name) {
            Some(edge) => {
                // 调用方拥有:独立 Box,持一份 Arc<GraphInner>。须 lmflow_input_free 释放。
                Box::into_raw(Box::new(InputHandle { graph: inner, edge })) as *mut LMFlowInput
            }
            None => {
                last_error::set(&format!("graph input port `{name}` does not exist"));
                std::ptr::null_mut()
            }
        }
    })
}

unsafe fn input_ref<'a>(i: *mut LMFlowInput) -> Option<&'a InputHandle> {
    if i.is_null() {
        None
    } else {
        Some(&*(i as *const InputHandle))
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_send(i: *mut LMFlowInput, pkt: LMFlowPacket) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        match input_ref(i) {
            Some(h) => to_status(h.graph.send_by_edge(h.edge, p, true)),
            None => fail(Error::InvalidArg("input handle is null".into())),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_try_send(i: *mut LMFlowInput, pkt: LMFlowPacket) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        match input_ref(i) {
            Some(h) => to_status(h.graph.send_by_edge(h.edge, p, false)),
            None => fail(Error::InvalidArg("input handle is null".into())),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_close(i: *mut LMFlowInput) {
    guard_val((), || {
        if let Some(h) = input_ref(i) {
            h.graph.close_edge_pub(h.edge);
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_free(i: *mut LMFlowInput) {
    guard_val((), || {
        if !i.is_null() {
            // 调用方拥有:归还这份句柄(及其对引擎的 Arc)。图可能已 free,但句柄仍安全。
            drop(Box::from_raw(i as *mut InputHandle));
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_add_packet(
    g: *mut LMFlowGraph,
    port: *const c_char,
    pkt: LMFlowPacket,
) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        let Some(name) = cstr(port) else {
            return fail(Error::InvalidArg("port name is null".into()));
        };
        with_graph(g, |gr| {
            let inner = gr.inner();
            match inner.input_edge_by_name(name) {
                Some(e) => to_status(inner.send_by_edge(e, p, true)),
                None => fail(Error::NotFound(format!(
                    "graph input port `{name}` does not exist"
                ))),
            }
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_close_input(g: *mut LMFlowGraph, port: *const c_char) -> i32 {
    guard(|| {
        let Some(name) = cstr(port) else {
            return fail(Error::InvalidArg("port name is null".into()));
        };
        with_graph(g, |gr| to_status(gr.close_input(name)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_close_all_inputs(g: *mut LMFlowGraph) {
    guard_val((), || {
        let _ = with_graph(g, |gr| {
            gr.close_all_inputs();
            code::OK
        });
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_add_poller(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> *mut LMFlowPoller {
    lmflow_graph_add_poller_ex(g, port, false)
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_add_poller_ex(
    g: *mut LMFlowGraph,
    port: *const c_char,
    observe_timestamp_bounds: bool,
) -> *mut LMFlowPoller {
    guard_val(std::ptr::null_mut(), || {
        if observe_timestamp_bounds {
            last_error::set("observe_timestamp_bounds belongs to a later phase, not implemented in this version");
            return std::ptr::null_mut();
        }
        let Some(slot) = slot_mut(g) else {
            last_error::set("graph handle is null");
            return std::ptr::null_mut();
        };
        let Some(gr) = slot.graph.as_ref() else {
            last_error::set("graph not yet initialized");
            return std::ptr::null_mut();
        };
        let Some(name) = cstr(port) else {
            last_error::set("port name is null");
            return std::ptr::null_mut();
        };
        match gr.add_poller(name) {
            Ok(p) => {
                // 调用方拥有:独立 Box,持一份 Arc<GraphInner>。须 lmflow_poller_free 释放。
                Box::into_raw(Box::new(p)) as *mut LMFlowPoller
            }
            Err(e) => {
                last_error::set(&e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_add_poller_bounded(
    g: *mut LMFlowGraph,
    port: *const c_char,
    capacity: usize,
    overflow: i32,
) -> *mut LMFlowPoller {
    guard_val(std::ptr::null_mut(), || {
        let Some(slot) = slot_mut(g) else {
            last_error::set("graph handle is null");
            return std::ptr::null_mut();
        };
        let Some(gr) = slot.graph.as_ref() else {
            last_error::set("graph not yet initialized");
            return std::ptr::null_mut();
        };
        let Some(name) = cstr(port) else {
            last_error::set("port name is null");
            return std::ptr::null_mut();
        };
        let overflow = match overflow {
            0 => crate::graph::PollerOverflow::Block,
            1 => crate::graph::PollerOverflow::DropOldest,
            2 => crate::graph::PollerOverflow::DropNewest,
            3 => crate::graph::PollerOverflow::Latest,
            _ => {
                last_error::set(
                    "unknown poller overflow policy (valid: BLOCK / DROP_OLDEST / DROP_NEWEST / LATEST)",
                );
                return std::ptr::null_mut();
            }
        };
        match gr.add_poller_with_options(name, crate::graph::PollerOptions::new(capacity, overflow))
        {
            Ok(poller) => Box::into_raw(Box::new(poller)) as *mut LMFlowPoller,
            Err(error) => {
                last_error::set(&error.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

unsafe fn poller_ref<'a>(p: *mut LMFlowPoller) -> Option<&'a Poller> {
    if p.is_null() {
        None
    } else {
        Some(&*(p as *const Poller))
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_dropped_count(p: *mut LMFlowPoller) -> u64 {
    guard_val(0, || poller_ref(p).map_or(0, Poller::dropped_count))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_next(p: *mut LMFlowPoller, out: *mut LMFlowPacket) -> bool {
    guard_val(false, || match poller_ref(p) {
        Some(poller) => match poller.next() {
            Some(pkt) => {
                if !out.is_null() {
                    *out = own_packet(pkt);
                }
                true
            }
            None => false,
        },
        None => false,
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_try_next(
    p: *mut LMFlowPoller,
    out: *mut LMFlowPacket,
) -> bool {
    guard_val(false, || match poller_ref(p).and_then(|x| x.try_next()) {
        Some(pkt) => {
            if !out.is_null() {
                *out = own_packet(pkt);
            }
            true
        }
        None => false,
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_next_timeout(
    p: *mut LMFlowPoller,
    out: *mut LMFlowPacket,
    timeout_ms: i64,
) -> i32 {
    guard(|| {
        let Some(poller) = poller_ref(p) else {
            return fail(Error::InvalidArg("poller handle is null".into()));
        };
        match poller.next_timeout(std::time::Duration::from_millis(timeout_ms.max(0) as u64)) {
            Ok(Some(pkt)) => {
                if !out.is_null() {
                    *out = own_packet(pkt);
                }
                code::OK
            }
            Ok(None) => code::CLOSED,
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_free(p: *mut LMFlowPoller) {
    guard_val((), || {
        if !p.is_null() {
            // 调用方拥有:归还这份句柄(及其对引擎的 Arc)。图可能已 free,但句柄仍安全。
            drop(Box::from_raw(p as *mut Poller));
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_observe(
    g: *mut LMFlowGraph,
    port: *const c_char,
    cb: Option<unsafe extern "C" fn(*mut c_void, LMFlowPacket)>,
    user: *mut c_void,
) -> i32 {
    lmflow_graph_observe_ex(g, port, false, cb, user)
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_observe_ex(
    g: *mut LMFlowGraph,
    port: *const c_char,
    observe_timestamp_bounds: bool,
    cb: Option<unsafe extern "C" fn(*mut c_void, LMFlowPacket)>,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        if observe_timestamp_bounds {
            return fail(Error::Unsupported(
                "observe_timestamp_bounds belongs to a later phase".into(),
            ));
        }
        let Some(f) = cb else {
            return fail(Error::InvalidArg("callback is null".into()));
        };
        let Some(name) = cstr(port) else {
            return fail(Error::InvalidArg("port name is null".into()));
        };
        with_graph(g, |gr| to_status(gr.inner().add_observer(name, f, user)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_cancel(g: *mut LMFlowGraph) {
    guard_val((), || {
        let _ = with_graph(g, |gr| {
            gr.cancel();
            code::OK
        });
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_done(g: *mut LMFlowGraph) -> i32 {
    guard(|| with_graph(g, |gr| to_status(gr.wait_done())))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_done_timeout(g: *mut LMFlowGraph, ms: i64) -> i32 {
    guard(|| {
        with_graph(g, |gr| {
            to_status(gr.wait_done_timeout(std::time::Duration::from_millis(ms.max(0) as u64)))
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_until_idle(g: *mut LMFlowGraph) -> i32 {
    guard(|| with_graph(g, |gr| to_status(gr.wait_until_idle())))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_until_idle_timeout(g: *mut LMFlowGraph, ms: i64) -> i32 {
    guard(|| {
        with_graph(g, |gr| {
            to_status(
                gr.wait_until_idle_timeout(std::time::Duration::from_millis(ms.max(0) as u64)),
            )
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_pause(g: *mut LMFlowGraph) {
    guard_val((), || {
        if let Some(gr) = graph_of(g) {
            gr.pause();
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_resume(g: *mut LMFlowGraph) {
    guard_val((), || {
        if let Some(gr) = graph_of(g) {
            gr.resume();
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_last_error(g: *mut LMFlowGraph) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        match slot_mut(g).and_then(|s| s.graph.as_ref()) {
            Some(gr) => gr.inner().shared.error_cstr(),
            None => c"".as_ptr(),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_state(g: *mut LMFlowGraph) -> i32 {
    guard_val(0, || match slot_mut(g).and_then(|s| s.graph.as_ref()) {
        Some(gr) => gr.state() as i32,
        None => State::Created as i32,
    })
}

// ---- 内省 ----

unsafe fn graph_of<'a>(g: *mut LMFlowGraph) -> Option<&'a Graph> {
    slot_mut(g).and_then(|s| s.graph.as_ref())
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_total_queued(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.total_queued())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_total_queued_bytes(g: *mut LMFlowGraph) -> u64 {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.total_queued_bytes())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_queue_depth(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        graph_of(g)
            .and_then(|gr| cstr(port).and_then(|p| gr.inner().queue_depth_by_name(p)))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_dropped_count(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> u64 {
    guard_val(0, || {
        graph_of(g)
            .and_then(|gr| cstr(port).and_then(|p| gr.inner().dropped_by_name(p)))
            .unwrap_or(0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_input_ports(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().num_input_ports())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_output_ports(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().num_output_ports())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_nodes(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || graph_of(g).map_or(0, |gr| gr.inner().nodes_len()))
}

fn graph_arena() -> &'static runtime::CStrArena {
    static A: std::sync::LazyLock<runtime::CStrArena> =
        std::sync::LazyLock::new(runtime::CStrArena::default);
    &A
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_input_port_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .input_port_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_output_port_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .output_port_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_name(g: *mut LMFlowGraph, idx: usize) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .node_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_num_input_ports(
    g: *mut LMFlowGraph,
    node_idx: usize,
) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |graph| graph.inner().node_input_ports_len(node_idx))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_input_port_name(
    g: *mut LMFlowGraph,
    node_idx: usize,
    port_idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|graph| {
                graph
                    .inner()
                    .node_input_port_name_at(node_idx, port_idx)
                    .map(|name| graph_arena().intern(name))
            })
            .unwrap_or(c"".as_ptr())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_dump(g: *mut LMFlowGraph) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        thread_local! {
            static BUF: std::cell::RefCell<std::ffi::CString> =
                std::cell::RefCell::new(std::ffi::CString::default());
        }
        let text = graph_of(g).map_or_else(|| "(uninitialized)".to_string(), |gr| gr.dump());
        BUF.with(|b| {
            *b.borrow_mut() = std::ffi::CString::new(text).unwrap_or_default();
            b.borrow().as_ptr()
        })
    })
}

/// 拓扑的 Graphviz DOT 导出。返回值同 `dump`:线程局部缓冲,调用方不得 free。
///
/// `with_stats != 0` 时在节点标签上标出运行统计并按平均延迟上热力图(见
/// `Graph::to_dot_with_stats`);可在运行期间随时调用。
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_to_dot(
    g: *mut LMFlowGraph,
    with_stats: bool,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        thread_local! {
            static BUF: std::cell::RefCell<std::ffi::CString> =
                std::cell::RefCell::new(std::ffi::CString::default());
        }
        // 未初始化图 → 合法的空 digraph(而非文本占位符),便于直接喂 graphviz。
        let text = graph_of(g).map_or_else(
            || "digraph lmflow {\n}\n".to_string(),
            |gr| {
                if with_stats {
                    gr.to_dot_with_stats()
                } else {
                    gr.to_dot()
                }
            },
        );
        BUF.with(|b| {
            *b.borrow_mut() = std::ffi::CString::new(text).unwrap_or_default();
            b.borrow().as_ptr()
        })
    })
}

#[repr(C)]
pub struct LMFlowNodeStats {
    pub struct_size: u32,
    pub reserved0: u32,
    pub node_name: *const c_char,
    pub kernel_name: *const c_char,
    pub running: bool,
    pub running_for_us: i64,
    pub processed: u64,
    pub errors: u64,
    pub total_process_us: i64,
    pub max_process_us: i64,
    pub packets_in: u64,
    pub packets_out: u64,
    pub peak_queue_depth: usize,
    pub queued: usize,
}

#[repr(C)]
pub struct LMFlowInputQueueStats {
    pub struct_size: u32,
    pub reserved0: u32,
    pub node_name: *const c_char,
    pub port_name: *const c_char,
    pub producer_name: *const c_char,
    pub packet_capacity: usize,
    pub byte_capacity: u64,
    pub queued_packets: usize,
    pub queued_bytes: u64,
    pub reserved_packets: usize,
    pub reserved_bytes: u64,
    pub peak_queued_packets: usize,
    pub peak_queued_bytes: u64,
    pub blocked: bool,
    pub reserved1: [u8; 7],
    pub blocked_for_us: u64,
    pub block_events: u64,
    pub total_blocked_us: u64,
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_stats(
    g: *mut LMFlowGraph,
    idx: usize,
    out: *mut LMFlowNodeStats,
) -> bool {
    guard_val(false, || {
        if out.is_null() {
            return false;
        }
        let declared = (*out).struct_size as usize;
        if declared < std::mem::size_of::<LMFlowNodeStats>() {
            last_error::set(
                "LMFlowNodeStats.struct_size too small -- set it to sizeof(LMFlowNodeStats)",
            );
            return false;
        }
        let Some(gr) = graph_of(g) else { return false };
        let Some(s) = gr.node_stats(idx) else {
            return false;
        };
        std::ptr::write(
            out,
            LMFlowNodeStats {
                struct_size: std::mem::size_of::<LMFlowNodeStats>() as u32,
                reserved0: 0,
                node_name: graph_arena().intern(&s.node_name),
                kernel_name: graph_arena().intern(&s.kernel_name),
                running: s.running,
                running_for_us: s.running_for_us,
                processed: s.processed,
                errors: s.errors,
                total_process_us: s.total_process_us,
                max_process_us: s.max_process_us,
                packets_in: s.packets_in,
                packets_out: s.packets_out,
                peak_queue_depth: s.peak_queue_depth,
                queued: s.queued,
            },
        );
        true
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_input_queue_stats(
    g: *mut LMFlowGraph,
    node_idx: usize,
    port_idx: usize,
    out: *mut LMFlowInputQueueStats,
) -> bool {
    guard_val(false, || {
        if out.is_null() {
            return false;
        }
        let declared = (*out).struct_size as usize;
        if declared < std::mem::size_of::<LMFlowInputQueueStats>() {
            last_error::set(
                "LMFlowInputQueueStats.struct_size too small -- set it to sizeof(LMFlowInputQueueStats)",
            );
            return false;
        }
        let Some(graph) = graph_of(g) else {
            return false;
        };
        let Some(stats) = graph.input_queue_stats(node_idx, port_idx) else {
            return false;
        };
        std::ptr::write(
            out,
            LMFlowInputQueueStats {
                struct_size: std::mem::size_of::<LMFlowInputQueueStats>() as u32,
                reserved0: 0,
                node_name: graph_arena().intern(&stats.node_name),
                port_name: graph_arena().intern(&stats.port_name),
                producer_name: stats
                    .producer_name
                    .as_deref()
                    .map_or(c"".as_ptr(), |name| graph_arena().intern(name)),
                packet_capacity: stats.packet_capacity.unwrap_or(0),
                byte_capacity: stats.byte_capacity.unwrap_or(0),
                queued_packets: stats.queued_packets,
                queued_bytes: stats.queued_bytes,
                reserved_packets: stats.reserved_packets,
                reserved_bytes: stats.reserved_bytes,
                peak_queued_packets: stats.peak_queued_packets,
                peak_queued_bytes: stats.peak_queued_bytes,
                blocked: stats.blocked,
                reserved1: [0; 7],
                blocked_for_us: stats.blocked_for_us,
                block_events: stats.block_events,
                total_blocked_us: stats.total_blocked_us,
            },
        );
        true
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_value(
    g: *mut LMFlowGraph,
    name: *const c_char,
) -> i64 {
    guard_val(0, || {
        graph_of(g)
            .and_then(|gr| cstr(name).map(|n| gr.inner().shared.counter_value(n)))
            .unwrap_or(0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_count(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.counter_names().len())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .shared
                    .counter_names()
                    .get(idx)
                    .map(|n| graph_arena().intern(n))
            })
            .unwrap_or(c"".as_ptr())
    })
}
