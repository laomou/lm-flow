#![allow(clippy::missing_safety_doc)]
use super::*;

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
pub unsafe extern "C" fn lmflow_graph_pump_step(g: *mut LMFlowGraph) -> bool {
    guard_val(false, || {
        let Some(slot) = slot_mut(g) else {
            last_error::set("graph handle is null");
            return false;
        };
        let Some(graph) = slot.graph.as_ref() else {
            last_error::set("graph not yet initialized");
            return false;
        };
        graph.pump_step()
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_pump_steps(g: *mut LMFlowGraph, max_steps: usize) -> usize {
    guard_val(0, || {
        let Some(slot) = slot_mut(g) else {
            last_error::set("graph handle is null");
            return 0;
        };
        let Some(graph) = slot.graph.as_ref() else {
            last_error::set("graph not yet initialized");
            return 0;
        };
        graph.pump_steps(max_steps)
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_set_wakeup_callback(
    g: *mut LMFlowGraph,
    cb: Option<unsafe extern "C" fn(*mut c_void)>,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        with_graph(g, |graph| {
            if let Some(callback) = cb {
                let user = user as usize;
                graph.set_wakeup_callback(move || unsafe { callback(user as *mut c_void) });
            } else {
                graph.clear_wakeup_callback();
            }
            code::OK
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
