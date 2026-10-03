use lmflow::{register_kernel, Kernel, KernelContract, KernelCtx, KernelRunner, Packet, Timestamp};
use std::sync::{Arc, Once};
#[derive(Default)]
struct Probe;
impl Kernel for Probe {
    fn get_contract(c: &mut KernelContract) {
        c.output_type(0, lmflow::packet::type_id::I64);
    }
    fn open(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        if c.option_bool("fail_open", false) {
            return Err(c.fail("open failed"));
        }
        Ok(())
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.forward(0, 0)
    }
    fn close(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        match c.option_i64("mode", 0) {
            1 => Err(c.fail("close failed")),
            2 => c.emit(0, Packet::from_f64(1.0).at(Timestamp::post_stream())),
            _ => c.emit(0, Packet::from_i64(99).at(Timestamp::post_stream())),
        }
    }
}
fn runner() -> KernelRunner {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<Probe>("PendingCloseProbe").unwrap());
    KernelRunner::new("PendingCloseProbe", 1, 1).unwrap()
}
fn check_close(started: bool, mode: i64) {
    let mut r = runner();
    r.set_options_json(&format!("{{\"mode\": {mode}}}"))
        .unwrap();
    if started {
        r.process(vec![Some(Packet::from_i64(7))], Timestamp(0))
            .unwrap();
    }
    let payload = Arc::new(vec![0u8; 1024]);
    let weak = Arc::downgrade(&payload);
    r.add_input(0, Packet::new(payload)).unwrap();
    let result = r.close();
    if started && mode != 0 {
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains(if mode == 1 {
                "close failed"
            } else {
                "type mismatch"
            }),
            "{error}"
        );
    } else {
        result.unwrap();
    }
    assert!(weak.upgrade().is_none(), "pending input survives Close");
    // Completed output survives Close; only unprocessed input is discarded.
    if started {
        assert_eq!(r.try_output(0).unwrap().unwrap().as_i64(), Some(7));
        if mode == 0 {
            assert_eq!(r.try_output(0).unwrap().unwrap().as_i64(), Some(99));
        }
    }
    assert!(r.try_output(0).unwrap().is_none());
    r.add_input(0, Packet::from_i64(42).at(Timestamp(1)))
        .unwrap();
    r.process_pending(Timestamp(1)).unwrap();
    assert_eq!(r.try_output(0).unwrap().unwrap().as_i64(), Some(42));
    assert!(r.try_output(0).unwrap().is_none());
}
#[test]
fn close_before_open_discards_pending_input_and_preserves_port_slots() {
    check_close(false, 0);
}
#[test]
fn normal_close_discards_pending_input_and_preserves_completed_output() {
    check_close(true, 0);
}
#[test]
fn close_errors_still_release_pending_input() {
    check_close(true, 1);
    check_close(true, 2);
}
#[test]
fn close_after_failed_open_releases_pending_input_without_clearing_fatal_error() {
    let mut r = runner();
    r.set_options_json(r#"{"fail_open":true}"#).unwrap();
    let payload = Arc::new(vec![0u8; 1024]);
    let weak = Arc::downgrade(&payload);
    r.add_input(0, Packet::new(payload)).unwrap();
    let error = r.open().unwrap_err().to_string();
    r.close().unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(r.open().unwrap_err().to_string(), error);
}
