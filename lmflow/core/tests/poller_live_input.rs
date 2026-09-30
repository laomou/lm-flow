mod common;

use lmflow::{Error, Graph, Packet, Timestamp};
use std::sync::mpsc;
use std::time::Duration;

fn graph(delegated: bool) -> Graph {
    common::register_test_kernels();
    let executors = if delegated {
        "executors: [{name: worker, type: DelegatingExecutor}]"
    } else {
        "executors: [{name: worker, type: ThreadPoolExecutor, num_threads: 1}]"
    };
    Graph::from_yaml(&format!("{executors}\nnodes:\n  - {{kernel: PassThrough, executor: worker, input_ports: [in], output_ports: [out]}}\ninput_ports: [in]\noutput_ports: [out]\n")).unwrap()
}

#[test]
fn blocking_poller_waits_for_delayed_input_on_both_executors() {
    for delegated in [false, true] {
        let graph = graph(delegated);
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            tx.send(poller.next_result().map(|p| p.and_then(|p| p.as_i64())))
                .unwrap();
        });
        let early = rx.recv_timeout(Duration::from_millis(30));
        graph
            .input("in")
            .unwrap()
            .send(Packet::from_i64(42).at(Timestamp(0)))
            .unwrap();
        graph.close_all_inputs();
        let was_waiting = early.is_err();
        let result = early.or_else(|_| rx.recv_timeout(Duration::from_secs(2)));
        // Unblock the reader even if this regression fails.
        graph.cancel();
        reader.join().unwrap();
        assert!(was_waiting, "temporarily idle input was reported as closed");
        assert_eq!(result.unwrap().unwrap(), Some(42));
    }
}

#[test]
fn idle_input_times_out_then_accepts_later_data() {
    let graph = graph(false);
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    assert!(matches!(
        poller.next_timeout(Duration::from_millis(20)),
        Err(Error::Timeout)
    ));
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(7).at(Timestamp(0)))
        .unwrap();
    assert_eq!(
        poller
            .next_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .as_i64(),
        Some(7)
    );
    graph.close_all_inputs();
    assert!(poller
        .next_timeout(Duration::from_secs(2))
        .unwrap()
        .is_none());
}

#[test]
fn idle_reader_wakes_on_input_close_or_cancellation() {
    for cancel in [false, true] {
        let graph = graph(false);
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            tx.send(poller.next_result()).unwrap();
        });
        let early = rx.recv_timeout(Duration::from_millis(30));
        if cancel {
            graph.cancel();
        } else {
            graph.close_all_inputs();
        }
        let was_waiting = early.is_err();
        let result = early.or_else(|_| rx.recv_timeout(Duration::from_secs(2)));
        graph.cancel();
        reader.join().unwrap();
        assert!(was_waiting);
        if cancel {
            assert!(matches!(result.unwrap(), Err(Error::Cancelled)));
        } else {
            assert!(result.unwrap().unwrap().is_none());
        }
    }
}
