use lmflow::ffi::*;
use lmflow::{Graph, KernelRunner, Packet, Timestamp};
use std::ffi::{c_void, CString};
use std::time::Duration;

unsafe extern "C" fn noop(_: *mut c_void, _: *mut LMFlowContext) -> i32 {
    0
}

unsafe extern "C" fn bad_emit(_: *mut c_void, ctx: *mut LMFlowContext) -> i32 {
    lmflow_ctx_emit(ctx, 0, lmflow_packet_from_i64(123, 0));
    lmflow_ctx_emit(ctx, 9, lmflow_packet_from_i64(456, 0));
    0 // The ABI operation returns void; an otherwise successful callback must still fail.
}

unsafe extern "C" fn bad_forward(_: *mut c_void, ctx: *mut LMFlowContext) -> i32 {
    lmflow_ctx_emit(ctx, 0, lmflow_packet_from_i64(123, 0));
    lmflow_ctx_forward(ctx, 9, 0);
    0
}

unsafe extern "C" fn fail_first(_: *mut c_void, ctx: *mut LMFlowContext) -> i32 {
    let timestamp = lmflow_ctx_input_timestamp(ctx);
    lmflow_ctx_emit(ctx, 0, lmflow_packet_from_i64(timestamp, timestamp));
    if timestamp == 0 {
        lmflow_ctx_forward(ctx, 0, 9);
    }
    0
}

type Callback = unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32;
fn register(name: &str, phase: &str, callback: Callback) {
    let name = CString::new(name).unwrap();
    let vtable = LMFlowKernelVTable {
        create: None,
        get_contract: None,
        destroy: None,
        open: Some(if phase == "open" { callback } else { noop }),
        process: Some(if phase == "process" { callback } else { noop }),
        close: Some(if phase == "close" { callback } else { noop }),
    };
    assert_eq!(
        unsafe { lmflow_register_kernel(name.as_ptr(), &vtable, std::ptr::null_mut()) },
        0
    );
}

fn graph(name: &str, policy: &str) -> Graph {
    Graph::from_yaml(&format!("nodes:\n  - {{name: n, kernel: {name}, input_ports: [in], output_ports: [out], on_error: {policy}}}\ninput_ports: [in]\noutput_ports: [out]\n")).unwrap()
}

#[test]
fn abi_output_errors_fail_each_graph_lifecycle_phase_without_partial_output() {
    for (operation, callback) in [
        ("emit", bad_emit as Callback),
        ("forward", bad_forward as Callback),
    ] {
        for phase in ["open", "process", "close"] {
            let name = format!("Bad_{operation}_{phase}");
            register(&name, phase, callback);
            let graph = graph(&name, "abort");
            let output = graph.add_poller("out").unwrap();
            let error = if phase == "open" {
                graph.start().unwrap_err()
            } else {
                graph.start().unwrap();
                graph
                    .input("in")
                    .unwrap()
                    .send(Packet::from_i64(0).at(Timestamp(0)))
                    .unwrap();
                graph.close_all_inputs();
                graph.wait_done_timeout(Duration::from_secs(2)).unwrap_err()
            };
            assert!(error.to_string().contains("out of range"), "{error}");
            assert!(
                output.try_next().is_none(),
                "partial output escaped during {phase}"
            );
        }
    }
}

#[test]
fn abi_output_errors_fail_kernel_runner() {
    register("BadRunnerOutput", "process", bad_emit);
    let mut runner = KernelRunner::new("BadRunnerOutput", 1, 1).unwrap();
    let error = runner
        .process(vec![Some(Packet::from_i64(0))], Timestamp(0))
        .unwrap_err();
    assert!(error.to_string().contains("out of range"));
    assert!(runner.try_output(0).unwrap().is_none());
}

#[test]
fn skipped_output_error_discards_partial_output_and_next_call_recovers() {
    register("SkipBadOutput", "process", fail_first);
    let graph = graph("SkipBadOutput", "skip");
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for value in 0..2 {
        input
            .send(Packet::from_i64(value).at(Timestamp(value)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(output.try_next().unwrap().as_i64(), Some(1));
    assert!(output.try_next().is_none());
}
