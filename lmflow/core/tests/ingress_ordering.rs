mod common;
use lmflow::{Error, Graph, Input, Packet, Poller, Timestamp};
use std::sync::{mpsc, Arc, Barrier, Mutex};
use std::time::Duration;

fn packet(ts: i64) -> Packet {
    Packet::from_i64(ts).at(Timestamp(ts))
}
fn graph() -> Graph {
    common::graph_from_yaml("nodes: [{kernel: PassThrough, input_ports: [in], output_ports: [out]}]\ninput_ports: [in]\noutput_ports: [in, out]").unwrap()
}
fn blocked_sender() -> (
    Graph,
    Poller,
    Arc<Barrier>,
    std::thread::JoinHandle<lmflow::Result<()>>,
) {
    let g = graph();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let a = entered.clone();
    let b = release.clone();
    g.observe("in", move |p| {
        if p.timestamp() == Timestamp(0) {
            a.wait();
            b.wait();
        }
    })
    .unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    g.pause();
    let input = g.input("in").unwrap();
    let sender = std::thread::spawn(move || input.send(packet(0)));
    entered.wait();
    (g, out, release, sender)
}
fn collect(out: &Poller) -> Vec<i64> {
    let mut result = vec![];
    while let Some(p) = out.try_next() {
        result.push(p.timestamp().0);
    }
    result
}
#[test]
fn concurrent_send_keeps_dispatch_order_and_try_send_does_not_wait() {
    let (g, out, release, first) = blocked_sender();
    let attempted = g.input("in").unwrap().try_send(packet(1));
    let input = g.input("in").unwrap();
    let (tx, rx) = mpsc::channel();
    let second = std::thread::spawn(move || {
        tx.send(input.send(packet(1))).unwrap();
    });
    let early = rx.recv_timeout(Duration::from_millis(30));
    release.wait();
    first.join().unwrap().unwrap();
    second.join().unwrap();
    assert!(matches!(attempted, Err(Error::WouldBlock)));
    assert!(
        early.is_err(),
        "second send must not overtake active dispatch"
    );
    rx.recv().unwrap().unwrap();
    g.close_all_inputs();
    g.resume();
    g.wait_done().unwrap();
    assert_eq!(collect(&out), vec![0, 1]);
}
#[test]
fn close_waits_until_accepted_packet_is_dispatched() {
    let (g, out, release, sender) = blocked_sender();
    let input = g.input("in").unwrap();
    let (tx, rx) = mpsc::channel();
    let closer = std::thread::spawn(move || {
        input.close();
        tx.send(()).unwrap();
    });
    let early = rx.recv_timeout(Duration::from_millis(30));
    release.wait();
    sender.join().unwrap().unwrap();
    closer.join().unwrap();
    assert!(early.is_err(), "close must not overtake dispatch");
    g.resume();
    g.wait_done().unwrap();
    assert_eq!(collect(&out), vec![0]);
    assert!(matches!(
        g.input("in").unwrap().send(packet(1)),
        Err(Error::Closed)
    ));
    g.reset().unwrap();
    g.start().unwrap();
    g.input("in").unwrap().send(packet(2)).unwrap();
    g.close_all_inputs();
    g.wait_done().unwrap();
    assert_eq!(collect(&out), vec![2]);
}
#[test]
fn callback_send_is_rejected_and_callback_close_is_deferred() {
    let g = graph();
    let input: Arc<Mutex<Option<Input>>> = Arc::new(Mutex::new(None));
    let callback_input = input.clone();
    g.observe("in", move |_| {
        let guard = callback_input.lock().unwrap();
        let input = guard.as_ref().unwrap();
        assert!(matches!(input.send(packet(1)), Err(Error::State(_))));
        input.close();
    })
    .unwrap();
    *input.lock().unwrap() = Some(g.input("in").unwrap());
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    g.input("in").unwrap().send(packet(0)).unwrap();
    g.wait_done().unwrap();
    assert_eq!(collect(&out), vec![0]);
    // Remove the observer's retained Input to avoid retaining GraphInner in a cycle.
    input.lock().unwrap().take();
}

#[test]
fn cancellation_unblocks_dispatch_waiting_on_a_bounded_input_poller() {
    use lmflow::{PollerOptions, PollerOverflow};
    let g = graph();
    let poller = g
        .add_poller_with_options(
            "in",
            PollerOptions::new(1, PollerOverflow::Block).with_block_timeout(None),
        )
        .unwrap();
    g.start().unwrap();
    g.pause();
    g.input("in").unwrap().send(packet(0)).unwrap();
    let input = g.input("in").unwrap();
    let sender = std::thread::spawn(move || input.send(packet(1)));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !poller.backpressure_stats().blocked && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let blocked = poller.backpressure_stats().blocked;
    g.cancel();
    let _ = sender.join().unwrap();
    assert!(blocked, "sender should have reached poller backpressure");
    g.close_all_inputs();
    assert!(matches!(g.wait_done(), Err(Error::Cancelled)));
}

#[test]
fn concurrent_callbacks_can_close_other_inputs_without_deadlock() {
    const CHILD: &str = "LMFLOW_CALLBACK_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Isolate a regression deadlock so a failing test cannot hang the suite.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "concurrent_callbacks_can_close_other_inputs_without_deadlock",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "callback-close child failed: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("concurrent callback close deadlocked");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    for close_all in [true, false] {
        let g = Arc::new(
            common::graph_from_yaml(
                "executors: [{name: host, type: DelegatingExecutor}]
nodes:
  - {kernel: PassThrough, executor: host, input_ports: [a], output_ports: [out_a]}
  - {kernel: PassThrough, executor: host, input_ports: [b], output_ports: [out_b]}
input_ports: [a, b]
output_ports: [a, b, out_a, out_b]",
            )
            .unwrap(),
        );
        let barrier = Arc::new(Barrier::new(2));
        let mut events = Vec::new();
        for (name, other) in [("a", "b"), ("b", "a")] {
            let weak = Arc::downgrade(&g);
            let barrier = barrier.clone();
            let seen = Arc::new(Mutex::new(Vec::new()));
            events.push(seen.clone());
            g.observe_with_timestamp_bounds(name, move |p| {
                seen.lock().unwrap().push((p.is_empty(), p.timestamp()));
                if p.is_empty() {
                    return;
                }
                barrier.wait();
                let graph = weak.upgrade().unwrap();
                if close_all {
                    graph.close_all_inputs();
                } else {
                    graph.input(other).unwrap().close();
                }
            })
            .unwrap();
        }
        let outputs = [
            g.add_poller("out_a").unwrap(),
            g.add_poller("out_b").unwrap(),
        ];
        for _ in 0..10 {
            g.start().unwrap();
            let senders: Vec<_> = ["a", "b"]
                .into_iter()
                .map(|name| {
                    let input = g.input(name).unwrap();
                    std::thread::spawn(move || input.send(packet(0)))
                })
                .collect();
            for sender in senders {
                sender.join().unwrap().unwrap();
            }
            g.wait_done_timeout(Duration::from_secs(2)).unwrap();
            for output in &outputs {
                assert_eq!(collect(output), vec![0]);
            }
            for seen in &events {
                assert_eq!(
                    *seen.lock().unwrap(),
                    vec![
                        (false, Timestamp(0)),
                        (true, Timestamp(1)),
                        (true, Timestamp::done())
                    ]
                );
                seen.lock().unwrap().clear();
            }
            g.reset().unwrap();
        }
    }
}
