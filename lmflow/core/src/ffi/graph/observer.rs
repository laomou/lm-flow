#![allow(clippy::missing_safety_doc)]
use super::*;

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
        let Some(f) = cb else {
            return fail(Error::InvalidArg("callback is null".into()));
        };
        let Some(name) = cstr(port) else {
            return fail(Error::InvalidArg("port name is null".into()));
        };
        with_graph(g, |gr| {
            to_status(
                gr.inner()
                    .add_observer(name, f, user, observe_timestamp_bounds),
            )
        })
    })
}
