use lmflow::{
    register_kernel, Error, Graph, Kernel, KernelContract, KernelCtx, Packet, State, Timestamp,
};
use std::sync::{Arc, Barrier, Once};
use std::time::Duration;

#[derive(Default)]
struct Probe;
impl Kernel for Probe {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.counter_add("probe.calls", 1);
        Ok(())
    }
    fn close(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.counter_add("probe.closes", 1);
        Ok(())
    }
}
#[derive(Default)]
struct Fail;
impl Kernel for Fail {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        Err(c.fail("stop marker"))
    }
}
struct Gates {
    entered: Barrier,
    release: Barrier,
    fail: bool,
}
#[derive(Default)]
struct Blocker;
impl Kernel for Blocker {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        let gates = c.input(0).unwrap().get::<Arc<Gates>>().unwrap().clone();
        c.counter_add("blocker.started", 1);
        gates.entered.wait();
        gates.release.wait();
        c.counter_add("blocker.completed", 1);
        if gates.fail {
            Err(c.fail("stop marker"))
        } else {
            Ok(())
        }
    }
}
fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<Probe>("StoppedProbe").unwrap();
        register_kernel::<Fail>("StoppedFail").unwrap();
        register_kernel::<Blocker>("StoppedBlocker").unwrap();
    });
}
#[test]
fn cancelled_delegated_claim_is_retired_without_process_and_can_reset() {
    register();
    let g = Graph::from_yaml(
        "executors: [{name: host, type: DelegatingExecutor}]
nodes: [{kernel: StoppedProbe, executor: host, input_ports: [in]}]
input_ports: [in]",
    )
    .unwrap();
    let input = g.input("in").unwrap();
    g.start().unwrap();
    let payload = Arc::new(42);
    let weak = Arc::downgrade(&payload);
    input.send(Packet::new(payload).at(Timestamp(0))).unwrap();
    assert_eq!(g.counter_value("probe.calls"), 0);
    g.cancel();
    assert!(matches!(
        g.wait_done_timeout(Duration::from_secs(2)),
        Err(Error::Cancelled)
    ));
    assert_eq!(g.state(), State::Terminated);
    assert_eq!(g.counter_value("probe.calls"), 0);
    assert_eq!(g.counter_value("probe.closes"), 1);
    assert!(weak.upgrade().is_none(), "skipped input must be released");
    g.reset().unwrap();
    g.start().unwrap();
    input.send(Packet::from_i64(7).at(Timestamp(0))).unwrap();
    input.close();
    g.wait_done().unwrap();
    assert_eq!(g.counter_value("probe.calls"), 1);
}
#[test]
fn graph_failure_skips_queued_callback_but_skip_policy_keeps_running() {
    register();
    for policy in ["abort", "skip"] {
        let g = Graph::from_yaml(&format!(
            "executors: [{{name: host, type: DelegatingExecutor}}]
nodes:
  - {{kernel: StoppedFail, executor: host, input_ports: [left], on_error: {policy}}}
  - {{kernel: StoppedProbe, executor: host, input_ports: [right]}}
input_ports: [left, right]"
        ))
        .unwrap();
        g.start().unwrap();
        for port in ["left", "right"] {
            g.input(port)
                .unwrap()
                .send(Packet::from_i64(1).at(Timestamp(0)))
                .unwrap();
        }
        g.close_all_inputs();
        let result = g.wait_done_timeout(Duration::from_secs(2));
        if policy == "abort" {
            assert!(
                matches!(result, Err(Error::Kernel(ref message)) if message.contains("stop marker"))
            );
            assert_eq!(g.counter_value("probe.calls"), 0);
        } else {
            result.unwrap();
            assert_eq!(g.counter_value("probe.calls"), 1);
        }
        assert_eq!(g.counter_value("probe.closes"), 1);
        assert_eq!(g.state(), State::Terminated);
    }
}
#[test]
fn pool_retires_queued_work_after_cancel_or_error_but_finishes_active_callback() {
    register();
    for fail in [false, true] {
        let g = Graph::from_yaml(
            "executors: [{name: pool, type: ThreadPoolExecutor, num_threads: 1}]
nodes:
  - {kernel: StoppedBlocker, executor: pool, input_ports: [active]}
  - {kernel: StoppedProbe, executor: pool, input_ports: [queued]}
input_ports: [active, queued]",
        )
        .unwrap();
        g.start().unwrap();
        let gates = Arc::new(Gates {
            entered: Barrier::new(2),
            release: Barrier::new(2),
            fail,
        });
        g.input("active")
            .unwrap()
            .send(Packet::new(gates.clone()).at(Timestamp(0)))
            .unwrap();
        gates.entered.wait();
        g.input("queued")
            .unwrap()
            .send(Packet::from_i64(1).at(Timestamp(0)))
            .unwrap();
        g.close_all_inputs();
        if !fail {
            g.cancel();
        }
        gates.release.wait();
        let result = g.wait_done_timeout(Duration::from_secs(2));
        if fail {
            assert!(matches!(result, Err(Error::Kernel(_))));
        } else {
            assert!(matches!(result, Err(Error::Cancelled)));
        }
        assert_eq!(g.counter_value("blocker.completed"), 1);
        assert_eq!(g.counter_value("probe.calls"), 0);
        assert_eq!(g.counter_value("probe.closes"), 1);
        assert_eq!(g.state(), State::Terminated);
    }
}
