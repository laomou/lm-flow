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

#[derive(Default)]
struct RepeatedRunner;

impl Kernel for RepeatedRunner {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.forward(0, 0)?;
        context.emit(1, Packet::from_i64(2))
    }

    fn close(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.emit(1, Packet::from_i64(99).at(Timestamp::post_stream()))
    }
}

#[test]
fn output_ports_survive_processing_close_and_reopen() {
    register_kernel::<RepeatedRunner>("RepeatedRunner").unwrap();
    let mut runner = KernelRunner::new("RepeatedRunner", 1, 2).unwrap();
    for cycle in 0..2 {
        for round in 0..3 {
            let value = cycle * 3 + round;
            let output = if round == 0 {
                runner.process(vec![Some(Packet::from_i64(value))], Timestamp(value))
            } else {
                runner.add_input(0, Packet::from_i64(value)).unwrap();
                runner.process_pending(Timestamp(value))
            }
            .unwrap();
            assert_eq!(output.len(), 2);
            assert_eq!(output[0][0].as_i64(), Some(value));
            assert_eq!(output[1][0].as_i64(), Some(2));
            assert_eq!(runner.try_output(0).unwrap().unwrap().as_i64(), Some(value));
            assert_eq!(runner.try_output(1).unwrap().unwrap().as_i64(), Some(2));
        }
        let output = runner.close().unwrap();
        assert_eq!(output.len(), 2);
        assert!(output[0].is_empty());
        assert_eq!(output[1][0].as_i64(), Some(99));
        assert_eq!(runner.try_output(1).unwrap().unwrap().as_i64(), Some(99));
        assert_eq!(runner.close().unwrap().len(), 2);
    }
}
