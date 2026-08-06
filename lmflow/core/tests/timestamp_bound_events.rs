mod common;

use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use lmflow::{register_kernel, Graph, Kernel, KernelCtx, OutputEvent, Packet, Timestamp};

#[derive(Default)]
struct DropWithExplicitBound;

impl Kernel for DropWithExplicitBound {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.set_next_bound(0, Timestamp(10));
        Ok(())
    }
}

fn register_kernels() {
    common::register_test_kernels();
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<DropWithExplicitBound>("DropWithExplicitBound").unwrap();
    });
}

fn one_node(kernel: &str) -> String {
    format!(
        "nodes:\n  - {{ name: k, kernel: {kernel}, input_ports: [in], output_ports: [out] }}\n\
         input_ports: [in]\noutput_ports: [out]\n"
    )
}

fn drain(poller: &lmflow::Poller) -> Vec<(bool, i64)> {
    let mut events = Vec::new();
    while let Some(packet) = poller.next() {
        events.push((packet.is_empty(), packet.timestamp().0));
    }
    events
}

#[test]
fn typed_events_distinguish_packets_bounds_and_done() {
    register_kernels();
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    let poller = graph.add_poller_with_timestamp_bounds("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(7).at(Timestamp(4)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();

    match poller.next_event() {
        Some(OutputEvent::Packet(packet)) => assert_eq!(packet.as_i64(), Some(7)),
        event => panic!("expected packet event, got {event:?}"),
    }
    assert!(matches!(
        poller.next_event(),
        Some(OutputEvent::TimestampBound(Timestamp(5)))
    ));
    assert!(matches!(poller.next_event(), Some(OutputEvent::Done)));
    assert!(poller.next_event().is_none());
}

#[test]
fn poller_receives_data_implicit_bound_and_done_in_order() {
    register_kernels();
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    let poller = graph.add_poller_with_timestamp_bounds("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::from_i64(7).at(Timestamp(4))).unwrap();
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(
        drain(&poller),
        vec![(false, 4), (true, 5), (true, Timestamp::done().0)]
    );
}

#[test]
fn consecutive_packets_publish_monotonic_bounds_without_duplicates() {
    register_kernels();
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    let poller = graph.add_poller_with_timestamp_bounds("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::from_i64(1).at(Timestamp(4))).unwrap();
    input.send(Packet::from_i64(2).at(Timestamp(7))).unwrap();
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(
        drain(&poller),
        vec![
            (false, 4),
            (true, 5),
            (false, 7),
            (true, 8),
            (true, Timestamp::done().0),
        ]
    );
}

#[test]
fn ordinary_poller_does_not_receive_bound_events() {
    register_kernels();
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(7).at(Timestamp(4)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(drain(&poller), vec![(false, 4)]);
}

#[test]
fn explicit_bound_is_published_when_kernel_emits_nothing() {
    register_kernels();
    let graph = Graph::from_yaml(&one_node("DropWithExplicitBound")).unwrap();
    let poller = graph.add_poller_with_timestamp_bounds("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(4)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(
        drain(&poller),
        vec![(true, 10), (true, Timestamp::done().0)]
    );
}

#[test]
fn observer_receives_bound_events_and_reset_restarts_sequence() {
    register_kernels();
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observer_seen = seen.clone();
    graph
        .observe_with_timestamp_bounds("out", move |packet| {
            observer_seen
                .lock()
                .expect("observer lock poisoned")
                .push((packet.is_empty(), packet.timestamp().0));
        })
        .unwrap();

    for round in 0..2 {
        graph.start().unwrap();
        graph
            .input("in")
            .unwrap()
            .send(Packet::from_i64(round).at(Timestamp(2)))
            .unwrap();
        graph.close_all_inputs();
        graph.wait_done_timeout(Duration::from_secs(5)).unwrap();
        if round == 0 {
            graph.reset().unwrap();
        }
    }

    assert_eq!(
        *seen.lock().expect("observer lock poisoned"),
        vec![
            (false, 2),
            (true, 3),
            (true, Timestamp::done().0),
            (false, 2),
            (true, 3),
            (true, Timestamp::done().0),
        ]
    );
}
