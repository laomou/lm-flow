use lmflow::packet::{Builtin, Payload};
use lmflow::{register_kernel, Kernel, KernelContract, KernelCtx, KernelRunner, Packet, Timestamp};
use std::sync::{Arc, Once};

#[derive(Default)]
struct Consume;
impl Kernel for Consume {
    fn get_contract(c: &mut KernelContract) {
        c.output_type(0, lmflow::packet::type_id::I64);
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        match c.option_i64("mode", 0) {
            1 => {
                c.forward(0, 0)?;
                Err(c.fail("process failed after staging output"))
            }
            2 => c.forward(0, 0), // Native input violates the declared I64 output.
            _ => Ok(()),
        }
    }
}
#[derive(Default)]
struct Forward;
impl Kernel for Forward {
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.forward(0, 0)
    }
}
fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<Consume>("LifetimeConsume").unwrap();
        register_kernel::<Forward>("LifetimeForward").unwrap();
    });
}
fn run_release_case(mode: i64) {
    register();
    let mut runner = KernelRunner::new("LifetimeConsume", 1, 1).unwrap();
    runner
        .set_options_json(&format!("{{\"mode\": {mode}}}"))
        .unwrap();
    for timestamp in 0..2 {
        let payload = Arc::new(vec![0u8; 1024]);
        let weak = Arc::downgrade(&payload);
        let result = runner.process(vec![Some(Packet::new(payload))], Timestamp(timestamp));
        if mode == 0 {
            assert!(result.unwrap()[0].is_empty());
        } else {
            let error = result.unwrap_err().to_string();
            let expected = if mode == 1 {
                "process failed"
            } else {
                "type mismatch"
            };
            assert!(error.contains(expected), "{error}");
        }
        assert!(runner.try_output(0).unwrap().is_none());
        assert!(
            weak.upgrade().is_none(),
            "completed input is still retained in mode {mode}"
        );
    }
}
#[test]
fn consumed_input_is_released_on_success() {
    run_release_case(0);
}
#[test]
fn input_and_partial_output_are_released_on_callback_error() {
    run_release_case(1);
}
#[test]
fn input_and_partial_output_are_released_on_output_type_error() {
    run_release_case(2);
}
#[test]
fn drained_forwarded_output_is_exclusive_and_mutates_without_copying() {
    register();
    let mut runner = KernelRunner::new("LifetimeForward", 1, 1).unwrap();
    let bytes = vec![7u8; 1024];
    let address = bytes.as_ptr();
    runner
        .add_input(0, Packet::from_builtin(Builtin::Bytes(bytes)))
        .unwrap();
    let returned = runner.process_pending(Timestamp(0)).unwrap();
    assert_eq!(returned[0].len(), 1);
    let mut output = runner.try_output(0).unwrap().unwrap();
    assert!(runner.try_output(0).unwrap().is_none());
    drop(returned);
    assert_eq!(output.ref_count(), 1, "the caller is now the only owner");
    assert!(
        matches!(output.payload(), Some(Payload::Builtin(Builtin::Bytes(bytes))) if bytes[0] == 7)
    );
    let Builtin::Bytes(bytes) = output.make_mutable_builtin().unwrap() else {
        panic!("expected bytes");
    };
    assert_eq!(bytes.as_ptr(), address, "write must not copy the buffer");
    bytes[0] = 9;
    assert_eq!(bytes[0], 9);
}
