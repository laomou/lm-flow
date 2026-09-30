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
