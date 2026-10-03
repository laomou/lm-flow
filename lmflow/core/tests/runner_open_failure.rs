use lmflow::{register_kernel, Kernel, KernelContract, KernelCtx, KernelRunner, Packet, Timestamp};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Once,
};
#[derive(Default)]
struct Counts {
    opens: AtomicUsize,
    processes: AtomicUsize,
    closes: AtomicUsize,
}
#[derive(Default)]
struct Probe {
    counts: Option<Arc<Counts>>,
}
impl Kernel for Probe {
    fn get_contract(c: &mut KernelContract) {
        c.require_side_packet("counts");
        c.output_type(0, lmflow::packet::type_id::I64);
    }
    fn open(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        let counts = c
            .side_packet("counts")
            .unwrap()
            .get::<Arc<Counts>>()
            .unwrap()
            .clone();
        counts.opens.fetch_add(1, Ordering::SeqCst);
        self.counts = Some(counts);
        if c.option_bool("wrong_type", false) {
            return c.emit(0, Packet::from_f64(1.0).at(Timestamp(-1)));
        }
        c.emit(0, Packet::from_i64(1).at(Timestamp(-1)))?;
        if c.option_bool("fail", false) {
            return Err(c.fail("initialization failed"));
        }
        Ok(())
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        self.counts
            .as_ref()
            .unwrap()
            .processes
            .fetch_add(1, Ordering::SeqCst);
        c.emit(0, Packet::from_i64(2))
    }
    fn close(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        self.counts
            .as_ref()
            .unwrap()
            .closes
            .fetch_add(1, Ordering::SeqCst);
        c.emit(0, Packet::from_i64(3).at(Timestamp(1)))
    }
}
fn runner() -> KernelRunner {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<Probe>("RunnerOpenFailureProbe").unwrap());
    KernelRunner::new("RunnerOpenFailureProbe", 0, 1).unwrap()
}
#[test]
fn invalid_open_output_remains_fatal_but_close_still_cleans_up_once() {
    let counts = Arc::new(Counts::default());
    let mut r = runner();
    r.set_side_packet("counts", Packet::new(counts.clone()))
        .unwrap();
    r.set_options_json(r#"{"wrong_type": true}"#).unwrap();
    let error = r.open().unwrap_err().to_string();
    assert!(error.contains("type mismatch"));
    assert_eq!(r.open().unwrap_err().to_string(), error);
    assert_eq!(
        r.process(vec![], Timestamp(0)).unwrap_err().to_string(),
        error
    );
    assert_eq!(r.close().unwrap_err().to_string(), error);
    assert_eq!(r.open().unwrap_err().to_string(), error);
    assert!(r.try_output(0).unwrap().is_none());
    drop(r);
    assert_eq!(counts.opens.load(Ordering::SeqCst), 1);
    assert_eq!(counts.processes.load(Ordering::SeqCst), 0);
    assert_eq!(counts.closes.load(Ordering::SeqCst), 1);
}
#[test]
fn drop_cleans_up_after_open_output_validation_failure() {
    let counts = Arc::new(Counts::default());
    {
        let mut r = runner();
        r.set_side_packet("counts", Packet::new(counts.clone()))
            .unwrap();
        r.set_options_json(r#"{"wrong_type": true}"#).unwrap();
        assert!(r.open().is_err());
    }
    assert_eq!(counts.closes.load(Ordering::SeqCst), 1);
}
#[test]
fn failed_open_callback_cannot_be_retried_or_processed() {
    let counts = Arc::new(Counts::default());
    let mut r = runner();
    r.set_side_packet("counts", Packet::new(counts.clone()))
        .unwrap();
    r.set_options_json(r#"{"fail": true}"#).unwrap();
    let error = r.process(vec![], Timestamp(0)).unwrap_err().to_string();
    assert!(error.contains("initialization failed"));
    assert_eq!(r.open().unwrap_err().to_string(), error);
    assert_eq!(
        r.process(vec![], Timestamp(0)).unwrap_err().to_string(),
        error
    );
    assert!(r.try_output(0).unwrap().is_none());
    assert_eq!(counts.opens.load(Ordering::SeqCst), 1);
    assert_eq!(counts.processes.load(Ordering::SeqCst), 0);
}
#[test]
fn missing_side_packet_is_recoverable_and_successful_close_allows_reopen() {
    let counts = Arc::new(Counts::default());
    let mut r = runner();
    assert!(r
        .open()
        .unwrap_err()
        .to_string()
        .contains("missing required side packet"));
    r.set_side_packet("counts", Packet::new(counts.clone()))
        .unwrap();
    for _ in 0..2 {
        r.open().unwrap();
        r.process(vec![], Timestamp(0)).unwrap();
        r.close().unwrap();
        for expected in [1, 2, 3] {
            assert_eq!(r.try_output(0).unwrap().unwrap().as_i64(), Some(expected));
        }
        assert!(r.try_output(0).unwrap().is_none());
    }
    assert_eq!(counts.opens.load(Ordering::SeqCst), 2);
    assert_eq!(counts.closes.load(Ordering::SeqCst), 2);
}
