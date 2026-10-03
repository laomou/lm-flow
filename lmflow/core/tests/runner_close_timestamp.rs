use lmflow::{register_kernel, Graph, Kernel, KernelCtx, KernelRunner, Packet, Timestamp};
use std::sync::{Arc, Mutex, Once};
#[derive(Default)]
struct Probe;
impl Kernel for Probe {
    fn process(&mut self, _: &mut KernelCtx) -> lmflow::Result<()> {
        Ok(())
    }
    fn close(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        if let Some(packet) = c.side_packet("timestamps") {
            packet
                .get::<Arc<Mutex<Vec<Timestamp>>>>()
                .unwrap()
                .lock()
                .unwrap()
                .push(c.input_timestamp());
        }
        if c.input_timestamp() == Timestamp::done() {
            c.emit(0, Packet::from_i64(42).at(Timestamp::post_stream()))?;
        }
        Ok(())
    }
}
fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<Probe>("RunnerCloseTimestampProbe").unwrap());
}
#[test]
fn runner_close_matches_graph_close_and_flushes_tail_output() {
    register();
    let graph = Graph::from_yaml("nodes: [{kernel: RunnerCloseTimestampProbe, input_ports: [in], output_ports: [out]}]\ninput_ports: [in]\noutput_ports: [out]").unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    let expected = output.try_next().unwrap();
    let mut runner = KernelRunner::new("RunnerCloseTimestampProbe", 1, 1).unwrap();
    for _ in 0..2 {
        runner.process(vec![None], Timestamp(7)).unwrap();
        let outputs = runner.close().unwrap();
        assert_eq!(outputs[0].len(), 1);
        assert_eq!(outputs[0][0].as_i64(), expected.as_i64());
        assert_eq!(outputs[0][0].timestamp(), expected.timestamp());
        assert_eq!(runner.try_output(0).unwrap().unwrap().as_i64(), Some(42));
        assert!(runner.try_output(0).unwrap().is_none());
    }
}
#[test]
fn explicit_and_drop_close_without_process_both_see_done() {
    register();
    for explicit in [false, true] {
        let timestamps = Arc::new(Mutex::new(Vec::<Timestamp>::new()));
        {
            let mut runner = KernelRunner::new("RunnerCloseTimestampProbe", 1, 1).unwrap();
            runner
                .set_side_packet("timestamps", Packet::new(timestamps.clone()))
                .unwrap();
            runner.open().unwrap();
            if explicit {
                runner.close().unwrap();
            }
        }
        assert_eq!(*timestamps.lock().unwrap(), vec![Timestamp::done()]);
    }
}
