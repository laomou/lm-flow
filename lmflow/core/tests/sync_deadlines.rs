use lmflow::{register_kernel, Error, Graph, Kernel, KernelContract, KernelCtx, Packet, Timestamp};
use std::sync::Once;
use std::time::Duration;

#[derive(Default)]
struct SlowDrop;
impl Kernel for SlowDrop {
    fn get_contract(contract: &mut KernelContract) {
        contract.input_any(0);
        contract.output_any(0);
    }
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        std::thread::sleep(Duration::from_millis(2));
        context.counter_add("processed", 1);
        Ok(())
    }
}
fn backlog() -> Graph {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<SlowDrop>("DeadlineSlowDrop").unwrap());
    Graph::from_yaml(
        "
 executors: [{name: host, type: DelegatingExecutor}]
 nodes: [{kernel: DeadlineSlowDrop, executor: host, input_ports: [in], output_ports: [out]}]
 input_ports: [in]
 output_ports: [out]",
    )
    .unwrap()
}
fn enqueue(graph: &Graph) {
    graph.start().unwrap();
    graph.pause();
    let input = graph.input("in").unwrap();
    for i in 0..100 {
        input.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
    }
    graph.resume();
}
#[test]
fn wait_done_checks_deadline_between_tasks_and_can_resume() {
    let graph = backlog();
    enqueue(&graph);
    graph.close_all_inputs();
    assert!(matches!(
        graph.wait_done_timeout(Duration::from_millis(10)),
        Err(Error::Timeout)
    ));
    assert!(graph.counter_value("processed") < 100);
    graph.wait_done().unwrap();
    assert_eq!(graph.counter_value("processed"), 100);
    graph.wait_done_timeout(Duration::ZERO).unwrap();
}
#[test]
fn wait_idle_checks_deadline_between_tasks_and_can_resume() {
    let graph = backlog();
    enqueue(&graph);
    assert!(matches!(
        graph.wait_until_idle_timeout(Duration::from_millis(10)),
        Err(Error::Timeout)
    ));
    assert!(graph.counter_value("processed") < 100);
    graph.wait_until_idle().unwrap();
    assert_eq!(graph.counter_value("processed"), 100);
    graph.wait_until_idle_timeout(Duration::ZERO).unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
}
#[test]
fn poller_checks_deadline_even_when_delegated_work_is_ready() {
    let graph = backlog();
    let poller = graph.add_poller("out").unwrap();
    enqueue(&graph);
    graph.close_all_inputs();
    assert!(matches!(
        poller.next_timeout(Duration::from_millis(10)),
        Err(Error::Timeout)
    ));
    assert!(graph.counter_value("processed") < 100);
    assert!(poller.next_result().unwrap().is_none());
    assert_eq!(graph.counter_value("processed"), 100);
    assert!(poller.next_timeout(Duration::ZERO).unwrap().is_none());
    graph.wait_done().unwrap();
}
#[test]
fn zero_timeout_does_not_execute_queued_callbacks() {
    let graph = backlog();
    let poller = graph.add_poller("out").unwrap();
    enqueue(&graph);
    graph.close_all_inputs();
    assert!(matches!(
        graph.wait_done_timeout(Duration::ZERO),
        Err(Error::Timeout)
    ));
    assert!(matches!(
        graph.wait_until_idle_timeout(Duration::ZERO),
        Err(Error::Timeout)
    ));
    assert!(matches!(
        poller.next_timeout(Duration::ZERO),
        Err(Error::Timeout)
    ));
    assert_eq!(graph.counter_value("processed"), 0);
    graph.cancel();
    assert!(matches!(graph.wait_done(), Err(Error::Cancelled)));
}
