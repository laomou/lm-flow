mod common;
use lmflow::{
    register_kernel, Error, Graph, Kernel, KernelContract, KernelCtx, Packet, State, Timestamp,
};
use std::sync::{Arc, Barrier};
use std::time::Duration;

fn pass_graph(expose_input: bool) -> Graph {
    common::graph_from_yaml(&format!(
        "executors: [{{name: host, type: DelegatingExecutor}}]
nodes: [{{kernel: PassThrough, executor: host, input_ports: [in], output_ports: [out]}}]
input_ports: [in]
output_ports: [{}]",
        if expose_input { "in, out" } else { "out" }
    ))
    .unwrap()
}
#[test]
fn cancel_releases_unclaimed_inputs_but_preserves_published_output_and_accounting() {
    let graph = pass_graph(false);
    let input = graph.input("in").unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    input.send(Packet::from_i64(42).at(Timestamp(0))).unwrap();
    while graph.pump_step() {}
    graph.pause();
    let payload = Arc::new(vec![0u8; 1024]);
    let weak = Arc::downgrade(&payload);
    input.send(Packet::new(payload).at(Timestamp(1))).unwrap();
    input
        .send(Packet::from_bytes(vec![0; 64]).at(Timestamp(2)))
        .unwrap();
    assert_eq!(graph.input_queue_stats(0, 0).unwrap().queued_packets, 2);
    assert_eq!(graph.input_queue_stats(0, 0).unwrap().queued_bytes, 64);
    assert_eq!(input.backpressure_stats().total_queued_packets, 3);
    graph.cancel();
    assert!(matches!(
        graph.wait_done_timeout(Duration::from_secs(2)),
        Err(Error::Cancelled)
    ));
    assert_eq!(graph.state(), State::Terminated);
    assert!(weak.upgrade().is_none());
    let stats = graph.input_queue_stats(0, 0).unwrap();
    assert_eq!(
        (
            stats.queued_packets,
            stats.queued_bytes,
            stats.reserved_packets
        ),
        (0, 0, 0)
    );
    assert_eq!(input.backpressure_stats().total_queued_packets, 1);
    assert_eq!(output.next_result().unwrap().unwrap().as_i64(), Some(42));
    assert_eq!(input.backpressure_stats().total_queued_packets, 0);
    graph.reset().unwrap();
    graph.start().unwrap();
    input.send(Packet::from_i64(7).at(Timestamp(0))).unwrap();
    input.close();
    graph.wait_done().unwrap();
    assert_eq!(output.next_result().unwrap().unwrap().as_i64(), Some(7));
    drop(graph);
    assert!(
        weak.upgrade().is_none(),
        "retained handles must not pin discarded inputs"
    );
}

#[derive(Default)]
struct Fail;
impl Kernel for Fail {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        Err(c.fail("abort marker"))
    }
}
#[test]
fn graph_error_discards_inputs_left_behind_the_failed_invocation() {
    register_kernel::<Fail>("AbortedInputFail").unwrap();
    let graph = Graph::from_yaml(
        "executors: [{name: host, type: DelegatingExecutor}]
nodes: [{kernel: AbortedInputFail, executor: host, input_ports: [in]}]
input_ports: [in]",
    )
    .unwrap();
    let input = graph.input("in").unwrap();
    graph.start().unwrap();
    graph.pause();
    input.send(Packet::from_i64(1).at(Timestamp(0))).unwrap();
    let payload = Arc::new(7);
    let weak = Arc::downgrade(&payload);
    input.send(Packet::new(payload).at(Timestamp(1))).unwrap();
    input
        .send(Packet::from_bytes(vec![0; 32]).at(Timestamp(2)))
        .unwrap();
    graph.resume();
    assert!(
        matches!(graph.wait_done_timeout(Duration::from_secs(2)), Err(Error::Kernel(ref message)) if message.contains("abort marker"))
    );
    assert!(weak.upgrade().is_none());
    assert_eq!(graph.input_queue_stats(0, 0).unwrap().queued_packets, 0);
    assert_eq!(graph.input_queue_stats(0, 0).unwrap().queued_bytes, 0);
    assert_eq!(input.backpressure_stats().total_queued_packets, 0);
}

#[test]
fn dispatch_in_progress_cannot_refill_inputs_after_forced_close() {
    let graph = pass_graph(true);
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let a = entered.clone();
    let b = release.clone();
    graph
        .observe("in", move |_| {
            a.wait();
            b.wait();
        })
        .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    let payload = Arc::new(7);
    let weak = Arc::downgrade(&payload);
    let sender = std::thread::spawn(move || input.send(Packet::new(payload).at(Timestamp(0))));
    entered.wait();
    graph.cancel();
    let result = graph.wait_done_timeout(Duration::from_secs(2));
    release.wait();
    sender.join().unwrap().unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(graph.state(), State::Terminated);
    assert!(weak.upgrade().is_none());
    assert_eq!(graph.input_queue_stats(0, 0).unwrap().queued_packets, 0);
    assert_eq!(
        graph
            .input("in")
            .unwrap()
            .backpressure_stats()
            .total_queued_packets,
        0
    );
}
