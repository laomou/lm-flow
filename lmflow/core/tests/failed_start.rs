use lmflow::{
    register_kernel, Error, Graph, Kernel, KernelContract, KernelCtx, Packet, State, Timestamp,
};
use std::sync::Once;
use std::time::Duration;

#[derive(Default)]
struct StartupProbe {
    attempted: bool,
}
impl Kernel for StartupProbe {
    fn get_contract(c: &mut KernelContract) {
        c.require_side_packet("ready");
        c.output_type(0, lmflow::packet::type_id::I64);
    }
    fn open(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.counter_add("opens", 1);
        if !self.attempted {
            self.attempted = true;
            if c.option_bool("fail", false) {
                return Err(c.fail("initialization failed"));
            }
            if c.option_bool("wrong_type", false) {
                return c.emit(0, Packet::from_f64(1.0).at(Timestamp(-1)));
            }
        }
        Ok(())
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.forward(0, 0)
    }
}
fn graph(options: &str) -> Graph {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<StartupProbe>("StartupProbe").unwrap());
    Graph::from_yaml(&format!("executors: [{{name: host, type: DelegatingExecutor}}]
nodes: [{{kernel: StartupProbe, executor: host, input_ports: [in], output_ports: [out], options: {options}}}]
input_ports: [in]
output_ports: [out]")).unwrap()
}
#[test]
fn fatal_open_failure_cannot_be_retried_as_success() {
    let g = graph("{fail: true}");
    g.set_side_packet("ready", Packet::from_i64(1)).unwrap();
    let first = g.start().unwrap_err().to_string();
    assert!(first.contains("initialization failed"), "{first}");
    assert_eq!(g.state(), State::Initialized);
    for _ in 0..2 {
        assert_eq!(g.start().unwrap_err().to_string(), first);
        assert_eq!(g.state(), State::Initialized);
        assert_eq!(g.counter_value("opens"), 1);
    }
}
#[test]
fn rejected_open_output_cannot_be_bypassed_by_skipping_open() {
    let g = graph("{wrong_type: true}");
    g.set_side_packet("ready", Packet::from_i64(1)).unwrap();
    let out = g.add_poller("out").unwrap();
    let first = g.start().unwrap_err().to_string();
    assert!(first.contains("type mismatch"), "{first}");
    assert_eq!(g.start().unwrap_err().to_string(), first);
    assert_eq!(g.state(), State::Initialized);
    assert_eq!(g.counter_value("opens"), 1);
    assert!(out.try_next().is_none());
}
#[test]
fn missing_side_packet_can_be_supplied_before_retry() {
    let g = graph("{}");
    let out = g.add_poller("out").unwrap();
    for _ in 0..2 {
        assert!(matches!(g.start(), Err(Error::InvalidArg(_))));
        assert_eq!(g.counter_value("opens"), 0);
    }
    g.set_side_packet("ready", Packet::from_i64(1)).unwrap();
    g.start().unwrap();
    g.input("in")
        .unwrap()
        .send(Packet::from_i64(42).at(Timestamp(0)))
        .unwrap();
    g.close_all_inputs();
    g.wait_done_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(out.try_next().unwrap().as_i64(), Some(42));
    assert!(out.try_next().is_none());
    assert_eq!(g.counter_value("opens"), 1);
}
#[test]
fn cancellation_before_start_does_not_run_open() {
    let g = graph("{}");
    g.set_side_packet("ready", Packet::from_i64(1)).unwrap();
    g.cancel();
    assert!(matches!(g.start(), Err(Error::Cancelled)));
    assert_eq!(g.counter_value("opens"), 0);
    assert_eq!(g.state(), State::Initialized);
}
