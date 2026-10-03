use lmflow::{
    register_kernel, Graph, Kernel, KernelContract, KernelCtx, KernelRunner, Packet, Timestamp,
};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

#[derive(Default)]
struct InitialOutput;
impl Kernel for InitialOutput {
    fn get_contract(c: &mut KernelContract) {
        c.output_type(0, lmflow::packet::type_id::I64);
    }
    fn open(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.counter_add("opens", 1);
        if c.option_bool("bound_only", false) {
            c.set_next_bound(0, Timestamp(1));
            return Ok(());
        }
        if c.option_bool("wrong_type", false) {
            return c.emit(0, Packet::from_f64(1.0).at(Timestamp(-1)));
        }
        let timestamp = c.option_i64("timestamp", -1);
        let count = c.option_i64("count", 1);
        for i in 0..count {
            c.emit(0, Packet::from_i64(42 + i).at(Timestamp(timestamp + i)))?;
        }
        if c.option_bool("fail", false) {
            return Err(c.fail("open failed"));
        }
        Ok(())
    }
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        if c.input(0).is_some() {
            c.forward(0, 0)
        } else {
            c.source_done();
            c.emit(0, Packet::from_i64(99))
        }
    }
}
fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<InitialOutput>("InitialOutput").unwrap());
}
fn graph(executor: &str, options: &str) -> Graph {
    register();
    Graph::from_yaml(&format!("executors: [{{name: host, type: {executor}}}]
nodes: [{{kernel: InitialOutput, executor: host, input_ports: [in], output_ports: [out], options: {options}}}]
input_ports: [in]
output_ports: [out]")).unwrap()
}
#[test]
fn initialization_reaches_poller_and_observer_before_process_and_is_not_replayed_on_reset() {
    for executor in ["DelegatingExecutor", "ThreadPoolExecutor"] {
        let g = graph(executor, "{}");
        let out = g.add_poller("out").unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callback_seen = seen.clone();
        g.observe("out", move |p| {
            callback_seen.lock().unwrap().push(p.as_i64().unwrap())
        })
        .unwrap();
        g.start().unwrap();
        assert_eq!(out.try_next().unwrap().as_i64(), Some(42));
        assert_eq!(*seen.lock().unwrap(), vec![42]);
        g.input("in")
            .unwrap()
            .send(Packet::from_i64(7).at(Timestamp(0)))
            .unwrap();
        g.close_all_inputs();
        g.wait_done_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(out.try_next().unwrap().as_i64(), Some(7));
        assert!(out.try_next().is_none());
        assert_eq!(*seen.lock().unwrap(), vec![42, 7]);
        g.reset().unwrap();
        g.start().unwrap();
        assert!(out.try_next().is_none());
        g.close_all_inputs();
        g.wait_done_timeout(Duration::from_secs(2)).unwrap();
    }
}
#[test]
fn downstream_open_output_precedes_forwarded_initialization_with_parallel_slots() {
    register();
    for executor in [
        "ThreadPoolExecutor, num_threads: 2",
        "ThreadPoolExecutor, num_threads: 4",
    ] {
        let g = Graph::from_yaml(&format!("executors: [{{name: host, type: {executor}}}]
nodes:
  - {{kernel: InitialOutput, executor: host, input_ports: [in], output_ports: [mid], max_in_flight: 2}}
  - {{kernel: InitialOutput, executor: host, input_ports: [mid], output_ports: [out], max_in_flight: 2, options: {{timestamp: -2}}, input_queues: {{packets: 1}}}}
input_ports: [in]
output_ports: [out]")).unwrap();
        let out = g.add_poller("out").unwrap();
        g.start().unwrap();
        g.input("in")
            .unwrap()
            .send(Packet::from_i64(7).at(Timestamp(0)))
            .unwrap();
        g.close_all_inputs();
        g.wait_done_timeout(Duration::from_secs(2)).unwrap();
        let mut stamps = vec![];
        while let Some(p) = out.try_next() {
            stamps.push(p.timestamp().0);
        }
        assert_eq!(stamps, vec![-2, -1, 0]);
    }
}
#[test]
fn source_initialization_does_not_consume_process_sequence_timestamp() {
    register();
    let g = Graph::from_yaml(
        "executors: [{name: host, type: ThreadPoolExecutor, num_threads: 1}]
nodes: [{kernel: InitialOutput, executor: host, output_ports: [out]}]
output_ports: [out]",
    )
    .unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    g.wait_done_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(out.try_next().unwrap().timestamp(), Timestamp(-1));
    assert_eq!(out.try_next().unwrap().timestamp(), Timestamp(0));
    assert!(out.try_next().is_none());
}
#[test]
fn initialization_bounds_are_published_without_data() {
    let g = graph("DelegatingExecutor", "{bound_only: true}");
    let out = g.add_poller_with_timestamp_bounds("out").unwrap();
    g.start().unwrap();
    let p = out.try_next().unwrap();
    assert!(p.is_empty());
    assert_eq!(p.timestamp(), Timestamp(1));
    g.close_all_inputs();
    g.wait_done().unwrap();
}
#[test]
fn invalid_or_failed_open_does_not_publish_partial_output() {
    for options in ["{wrong_type: true}", "{fail: true}"] {
        let g = graph("DelegatingExecutor", options);
        let out = g.add_poller("out").unwrap();
        assert!(g.start().is_err());
        assert!(out.try_next().is_none());
    }
}
#[test]
fn initialization_respects_internal_queue_capacity() {
    register();
    let g = Graph::from_yaml("executors: [{name: host, type: DelegatingExecutor}]
nodes:
  - {kernel: InitialOutput, executor: host, input_ports: [in], output_ports: [mid], options: {count: 2, timestamp: -2}}
  - {kernel: InitialOutput, executor: host, input_ports: [mid], output_ports: [out], input_queues: {packets: 1}, options: {bound_only: true}}
input_ports: [in]
output_ports: [mid, out]").unwrap();
    let out = g.add_poller("mid").unwrap();
    g.start().unwrap();
    g.close_all_inputs();
    let err = g
        .wait_done_timeout(Duration::from_secs(2))
        .unwrap_err()
        .to_string();
    assert!(err.contains("capacity"), "{err}");
    assert!(out.try_next().is_none());
}
#[test]
fn runner_collects_explicit_and_implicit_open_once_and_validates_types() {
    register();
    for explicit in [false, true] {
        let mut r = KernelRunner::new("InitialOutput", 1, 1).unwrap();
        if explicit {
            r.open().unwrap();
            r.open().unwrap();
            assert_eq!(r.try_output(0).unwrap().unwrap().as_i64(), Some(42));
            assert!(r.try_output(0).unwrap().is_none());
        }
        let packets = r
            .process(vec![Some(Packet::from_i64(7))], Timestamp(0))
            .unwrap();
        assert_eq!(packets[0].len(), 1);
        if !explicit {
            assert_eq!(r.try_output(0).unwrap().unwrap().as_i64(), Some(42));
        }
        assert_eq!(r.try_output(0).unwrap().unwrap().as_i64(), Some(7));
        assert!(r.try_output(0).unwrap().is_none());
    }
    for options in [r#"{"wrong_type": true}"#, r#"{"fail": true}"#] {
        let mut r = KernelRunner::new("InitialOutput", 1, 1).unwrap();
        r.set_options_json(options).unwrap();
        assert!(r.open().is_err());
        assert!(r.try_output(0).unwrap().is_none());
    }
}

#[test]
fn later_open_failure_discards_earlier_initialization() {
    register();
    let g = Graph::from_yaml(
        "nodes:
  - {kernel: InitialOutput, input_ports: [in], output_ports: [mid]}
  - {kernel: InitialOutput, input_ports: [mid], output_ports: [out], options: {fail: true}}
input_ports: [in]
output_ports: [mid, out]",
    )
    .unwrap();
    let mid = g.add_poller("mid").unwrap();
    let out = g.add_poller("out").unwrap();
    assert!(g.start().is_err());
    assert!(mid.try_next().is_none());
    assert!(out.try_next().is_none());
}

#[test]
fn cancellation_during_open_dispatch_releases_reserved_slots() {
    let g = Arc::new(graph("DelegatingExecutor", "{}"));
    let weak = Arc::downgrade(&g);
    g.observe("out", move |_| weak.upgrade().unwrap().cancel())
        .unwrap();
    g.start().unwrap();
    g.close_all_inputs();
    assert!(matches!(
        g.wait_done_timeout(Duration::from_secs(2)),
        Err(lmflow::Error::Cancelled)
    ));
    g.reset().unwrap();
}
