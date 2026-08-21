use lmflow::{register_kernel, Kernel, KernelContract, KernelCtx, KernelRunner, Packet, Timestamp};
use std::sync::Once;

#[derive(Default)]
struct RunnerScale {
    factor: i64,
}

impl Kernel for RunnerScale {
    fn get_contract(contract: &mut KernelContract) {
        contract.input_type(0, lmflow::packet::type_id::I64);
        contract.output_type(0, lmflow::packet::type_id::I64);
        contract.require_side_packet("bias");
    }

    fn open(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        self.factor = context.option_i64("factor", 1);
        Ok(())
    }

    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        let value = context.input(0).and_then(Packet::as_i64).unwrap();
        let bias = context
            .side_packet("bias")
            .and_then(Packet::as_i64)
            .unwrap();
        context.emit(0, Packet::from_i64(value * self.factor + bias))
    }
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<RunnerScale>("RunnerScale").unwrap());
}

#[test]
fn directly_drives_rust_kernel_lifecycle() {
    register();
    let mut runner = KernelRunner::new("RunnerScale", 1, 1).unwrap();
    runner.set_options_json(r#"{"factor":3}"#).unwrap();
    runner.set_side_packet("bias", Packet::from_i64(2)).unwrap();
    runner
        .add_input(0, Packet::from_i64(7).at(Timestamp(11)))
        .unwrap();
    runner.process_pending(Timestamp(11)).unwrap();

    let output = runner.try_output(0).unwrap().unwrap();
    assert_eq!(output.as_i64(), Some(23));
    assert_eq!(output.timestamp(), Timestamp(11));
    assert!(runner.try_output(0).unwrap().is_none());
    runner.close().unwrap();
}

#[test]
fn required_side_packet_is_checked_without_a_graph() {
    register();
    let mut runner = KernelRunner::new("RunnerScale", 1, 1).unwrap();
    let error = runner.open().unwrap_err().to_string();
    assert!(error.contains("bias"), "{error}");
}

#[test]
fn duplicate_pending_input_is_rejected() {
    register();
    let mut runner = KernelRunner::new("RunnerScale", 1, 1).unwrap();
    runner.add_input(0, Packet::from_i64(1)).unwrap();
    let error = runner
        .add_input(0, Packet::from_i64(2))
        .unwrap_err()
        .to_string();
    assert!(error.contains("already has a packet"), "{error}");
}
