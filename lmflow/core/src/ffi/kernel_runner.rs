use super::*;

unsafe fn runner_ref<'a>(p: *mut LMFlowKernelRunner) -> Option<&'a mut KernelRunnerHandle> {
    (!p.is_null()).then(|| &mut *(p as *mut KernelRunnerHandle))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_new(
    kernel: *const c_char,
    input_ports: usize,
    output_ports: usize,
) -> *mut LMFlowKernelRunner {
    guard_val(std::ptr::null_mut(), || {
        let Some(name) = cstr(kernel) else {
            last_error::set("kernel name is null");
            return std::ptr::null_mut();
        };
        match KernelRunner::new(name, input_ports, output_ports) {
            Ok(runner) => {
                Box::into_raw(Box::new(KernelRunnerHandle { runner })) as *mut LMFlowKernelRunner
            }
            Err(error) => {
                last_error::set(&error.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_free(p: *mut LMFlowKernelRunner) {
    guard_val((), || {
        if !p.is_null() {
            drop(Box::from_raw(p as *mut KernelRunnerHandle));
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_start(p: *mut LMFlowKernelRunner) -> i32 {
    guard(|| {
        let Some(handle) = runner_ref(p) else {
            return fail(Error::InvalidArg("kernel runner handle is null".into()));
        };
        to_status(handle.runner.open())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_set_options_json(
    p: *mut LMFlowKernelRunner,
    json: *const c_char,
) -> i32 {
    guard(|| {
        let Some(handle) = runner_ref(p) else {
            return fail(Error::InvalidArg("kernel runner handle is null".into()));
        };
        let Some(json) = cstr(json) else {
            return fail(Error::InvalidArg(
                "options JSON is null or invalid UTF-8".into(),
            ));
        };
        to_status(handle.runner.set_options_json(json))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_set_side_packet(
    p: *mut LMFlowKernelRunner,
    name: *const c_char,
    packet: LMFlowPacket,
) -> i32 {
    guard(|| {
        let owned = take_packet(packet);
        let Some(handle) = runner_ref(p) else {
            return fail(Error::InvalidArg("kernel runner handle is null".into()));
        };
        let Some(name) = cstr(name) else {
            return fail(Error::InvalidArg("side packet name is null".into()));
        };
        to_status(handle.runner.set_side_packet(name, owned))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_add_input(
    p: *mut LMFlowKernelRunner,
    port: usize,
    packet: LMFlowPacket,
) -> i32 {
    guard(|| {
        let owned = take_packet(packet);
        let Some(handle) = runner_ref(p) else {
            return fail(Error::InvalidArg("kernel runner handle is null".into()));
        };
        to_status(handle.runner.add_input(port, owned))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_process(
    p: *mut LMFlowKernelRunner,
    timestamp: i64,
) -> i32 {
    guard(|| {
        let Some(handle) = runner_ref(p) else {
            return fail(Error::InvalidArg("kernel runner handle is null".into()));
        };
        match handle
            .runner
            .process_pending(crate::timestamp::Timestamp(timestamp))
        {
            Ok(_) => code::OK,
            Err(error) => fail(error),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_try_next(
    p: *mut LMFlowKernelRunner,
    port: usize,
    out: *mut LMFlowPacket,
) -> i32 {
    guard(|| {
        let Some(handle) = runner_ref(p) else {
            return fail(Error::InvalidArg("kernel runner handle is null".into()));
        };
        match handle.runner.try_output(port) {
            Ok(Some(packet)) => {
                if !out.is_null() {
                    *out = own_packet(packet);
                }
                code::OK
            }
            Ok(None) => code::WOULD_BLOCK,
            Err(error) => fail(error),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_kernel_runner_close(p: *mut LMFlowKernelRunner) -> i32 {
    guard(|| {
        let Some(handle) = runner_ref(p) else {
            return fail(Error::InvalidArg("kernel runner handle is null".into()));
        };
        match handle.runner.close() {
            Ok(_) => code::OK,
            Err(error) => fail(error),
        }
    })
}
