use lmflow::{register_kernel, Graph, Kernel, KernelContract, KernelCtx, Packet, Timestamp};
use std::sync::{Arc, Mutex, Once};

#[derive(Default)]
struct EmitAndBound;
impl Kernel for EmitAndBound {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
        c.output_any(0);
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.forward(0, 0)?;
        c.set_next_bound(0, Timestamp(10));
        Ok(())
    }
}
#[derive(Default)]
struct Join;
impl Kernel for Join {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
        c.input_any(1);
        c.output_any(0);
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.emit(0, Packet::from_i64(42))
    }
}
fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<EmitAndBound>("EmitAndBound").unwrap();
        register_kernel::<Join>("BoundJoin").unwrap();
    });
}
fn drain(poller: &lmflow::Poller) -> Vec<(bool, i64)> {
    let mut events = vec![];
    while let Some(p) = poller.try_next() {
        events.push((p.is_empty(), p.timestamp().0));
    }
    events
}
#[test]
fn data_then_explicit_bound_reaches_pollers_and_observers_monotonically() {
    register();
    // Greater, equal, and lower explicit bounds relative to the implicit data bound.
    for ts in [0, 9, 20] {
        let graph = Graph::from_yaml(
            "executors: [{name: host, type: DelegatingExecutor}]
nodes: [{kernel: EmitAndBound, executor: host, input_ports: [in], output_ports: [out]}]
input_ports: [in]
output_ports: [out]",
        )
        .unwrap();
        let poller = graph.add_poller_with_timestamp_bounds("out").unwrap();
        let ordinary = graph.add_poller("out").unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let callback_seen = seen.clone();
        graph
            .observe_with_timestamp_bounds("out", move |p| {
                callback_seen
                    .lock()
                    .unwrap()
                    .push((p.is_empty(), p.timestamp().0));
            })
            .unwrap();
        graph.start().unwrap();
        graph
            .input("in")
            .unwrap()
            .send(Packet::from_i64(7).at(Timestamp(ts)))
            .unwrap();
        while graph.pump_step() {}
        let mut expected = vec![(false, ts), (true, ts + 1)];
        if ts < 9 {
            expected.push((true, 10));
        }
        assert_eq!(drain(&poller), expected);
        assert_eq!(*seen.lock().unwrap(), expected);
        assert_eq!(drain(&ordinary), vec![(false, ts)]);
        graph.close_all_inputs();
        graph.wait_done().unwrap();
        assert_eq!(drain(&poller), vec![(true, Timestamp::done().0)]);
    }
}
#[test]
fn explicit_bound_unblocks_join_before_producer_input_closes() {
    register();
    let graph = Graph::from_yaml(
        "executors: [{name: host, type: DelegatingExecutor}]
nodes:
  - {kernel: EmitAndBound, executor: host, input_ports: [left], output_ports: [middle]}
  - {kernel: BoundJoin, executor: host, input_ports: [middle, right], output_ports: [out]}
input_ports: [left, right]
output_ports: [out]",
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("left")
        .unwrap()
        .send(Packet::from_i64(0).at(Timestamp(0)))
        .unwrap();
    graph
        .input("right")
        .unwrap()
        .send(Packet::from_i64(5).at(Timestamp(5)))
        .unwrap();
    graph.close_input("right").unwrap();
    while graph.pump_step() {}
    assert_eq!(drain(&output), vec![(false, 0), (false, 5)]);
    graph.close_input("left").unwrap();
    graph.wait_done().unwrap();
    assert!(drain(&output).is_empty());
}
