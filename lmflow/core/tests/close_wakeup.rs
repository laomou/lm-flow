mod common;

use lmflow::{Graph, OutputEvent, State};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[test]
fn closing_idle_inputs_wakes_host_for_each_output() {
    common::register_test_kernels();
    let graph = Graph::from_yaml(
        "\
nodes:
  - {kernel: PassThrough, input_ports: [left], output_ports: [left_out]}
  - {kernel: PassThrough, input_ports: [right], output_ports: [right_out]}
input_ports: [left, right]
output_ports: [left_out, right_out]",
    )
    .unwrap();
    let left = graph.add_poller_with_timestamp_bounds("left_out").unwrap();
    let right = graph.add_poller_with_timestamp_bounds("right_out").unwrap();
    let wakeups = Arc::new(AtomicUsize::new(0));
    let callback_wakeups = wakeups.clone();
    graph.set_wakeup_callback(move || {
        callback_wakeups.fetch_add(1, Ordering::SeqCst);
    });
    graph.start().unwrap();
    while graph.pump_step() {}

    let before = wakeups.load(Ordering::SeqCst);
    graph.close_input("left").unwrap();
    assert!(
        wakeups.load(Ordering::SeqCst) > before,
        "idle close must wake the host"
    );
    while graph.pump_step() {}
    assert!(matches!(left.try_next_event(), Some(OutputEvent::Done)));
    assert_eq!(graph.state(), State::Running);

    let before = wakeups.load(Ordering::SeqCst);
    graph.input("right").unwrap().close();
    assert!(
        wakeups.load(Ordering::SeqCst) > before,
        "input-handle close must wake the host"
    );
    while graph.pump_step() {}
    assert!(matches!(right.try_next_event(), Some(OutputEvent::Done)));
    assert_eq!(graph.state(), State::Terminated);
}
