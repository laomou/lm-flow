mod common;
use lmflow::{register_kernel, Error, Graph, Kernel, KernelContract, KernelCtx, Packet, Timestamp};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

fn graph(executor: &str) -> Graph {
    common::graph_from_yaml(&format!(
        "executors: [{{name: host, type: {executor}}}]
nodes: [{{kernel: PassThrough, executor: host, input_ports: [in], output_ports: [out]}}]
input_ports: [in]
output_ports: [out]"
    ))
    .unwrap()
}

#[test]
fn retained_handles_are_closed_after_drop_for_both_executors() {
    for executor in ["ThreadPoolExecutor", "DelegatingExecutor"] {
        let graph = graph(executor);
        let input = graph.input("in").unwrap();
        let poller = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        // Leave queued input behind while scheduling is paused.
        graph.pause();
        input.send(Packet::from_i64(1).at(Timestamp(0))).unwrap();
        drop(graph);
        assert!(matches!(
            input.send(Packet::from_i64(2).at(Timestamp(1))),
            Err(Error::Closed | Error::Cancelled)
        ));
        assert!(poller.is_closed());
        assert!(poller.next_result().unwrap().is_none());
    }
}

#[test]
fn dropping_graph_wakes_waiting_poller() {
    let graph = graph("ThreadPoolExecutor");
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        done_tx
            .send(poller.next_timeout(Duration::from_secs(2)))
            .unwrap();
    });
    ready_rx.recv().unwrap();
    drop(graph);
    let result = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("drop must wake the poller");
    assert!(matches!(result, Ok(None) | Err(Error::Cancelled)));
    waiter.join().unwrap();
}

static CLOSES: AtomicUsize = AtomicUsize::new(0);
#[derive(Default)]
struct CloseProbe;
impl Kernel for CloseProbe {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
    }
    fn process(&mut self, _: &mut KernelCtx) -> lmflow::Result<()> {
        Ok(())
    }
    fn close(&mut self, _: &mut KernelCtx) -> lmflow::Result<()> {
        CLOSES.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[test]
fn close_runs_at_graph_drop_without_waiting_for_retained_input() {
    register_kernel::<CloseProbe>("DropCloseProbe").unwrap();
    let graph =
        Graph::from_yaml("nodes: [{kernel: DropCloseProbe, input_ports: [in]}]\ninput_ports: [in]")
            .unwrap();
    let input = graph.input("in").unwrap();
    graph.start().unwrap();
    drop(graph);
    assert_eq!(CLOSES.load(Ordering::SeqCst), 1);
    drop(input);
    assert_eq!(CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn completed_output_remains_readable_after_graph_drop() {
    let graph = graph("DelegatingExecutor");
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(42).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    drop(graph);
    assert!(poller.next_result().unwrap().is_some());
    assert!(poller.next_result().unwrap().is_none());
}
