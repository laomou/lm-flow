#![allow(clippy::missing_safety_doc)]
use super::*;

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
        let poller = if observe_timestamp_bounds {
            gr.add_poller_with_timestamp_bounds(name)
        } else {
            gr.add_poller(name)
        };
        match poller {
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
pub unsafe extern "C" fn lmflow_poller_next_status(
    p: *mut LMFlowPoller,
    out: *mut LMFlowPacket,
) -> i32 {
    guard(|| {
        let Some(poller) = poller_ref(p) else {
            return fail(Error::InvalidArg("poller handle is null".into()));
        };
        match poller.next_result() {
            Ok(Some(pkt)) => {
                if !out.is_null() {
                    *out = own_packet(pkt);
                }
                code::OK
            }
            Ok(None) => code::CLOSED,
            Err(error) => fail(error),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_try_next_status(
    p: *mut LMFlowPoller,
    out: *mut LMFlowPacket,
) -> i32 {
    guard(|| {
        let Some(poller) = poller_ref(p) else {
            return fail(Error::InvalidArg("poller handle is null".into()));
        };
        match poller.try_next_result() {
            Ok(Some(pkt)) => {
                if !out.is_null() {
                    *out = own_packet(pkt);
                }
                code::OK
            }
            Ok(None) => {
                if poller.is_closed() {
                    code::CLOSED
                } else {
                    code::WOULD_BLOCK
                }
            }
            Err(error) => fail(error),
        }
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
