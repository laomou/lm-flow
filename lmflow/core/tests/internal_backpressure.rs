use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lmflow::{register_kernel, Graph, Kernel, KernelContract, KernelCtx, Packet, Timestamp};

#[derive(Default)]
struct SlowPass;

impl Kernel for SlowPass {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        std::thread::sleep(Duration::from_millis(2));
        context.forward(0, 0)
    }
}

#[derive(Default)]
struct CountingSource {
    next: i64,
}

impl Kernel for CountingSource {
    fn get_contract(contract: &mut KernelContract) {
        contract.output_type(0, lmflow::packet::type_id::I64);
    }

    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        if self.next == 100 {
            context.source_done();
            return Ok(());
        }
        context.emit(0, Packet::from_i64(self.next))?;
        self.next += 1;
        Ok(())
    }
}

#[derive(Default)]
struct Duplicate;

impl Kernel for Duplicate {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        let packet = context.input(0).cloned().unwrap();
        context.emit(0, packet.clone())?;
        context.emit(1, packet)
    }
}

#[derive(Default)]
struct Burst;

impl Kernel for Burst {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        let packet = context.input(0).cloned().unwrap();
        context.emit(0, packet.clone())?;
        context.emit(0, packet)
    }
}

#[derive(Default)]
struct EmitOnClose;

impl Kernel for EmitOnClose {
    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.forward(0, 0)
    }

    fn close(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.emit(0, Packet::from_i64(99).at(Timestamp(1)))
    }
}

#[derive(Default)]
struct JoinCount;

static JOINED: AtomicUsize = AtomicUsize::new(0);

impl Kernel for JoinCount {
    fn process(&mut self, _context: &mut KernelCtx) -> lmflow::Result<()> {
        JOINED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct CloseJoinCount;

static CLOSE_JOINED: AtomicUsize = AtomicUsize::new(0);

impl Kernel for CloseJoinCount {
    fn process(&mut self, _context: &mut KernelCtx) -> lmflow::Result<()> {
        CLOSE_JOINED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct PortJoinCount;

impl Kernel for PortJoinCount {
    fn process(&mut self, _context: &mut KernelCtx) -> lmflow::Result<()> {
        Ok(())
    }
}

fn register_test_kernels() {
    let _ = register_kernel::<SlowPass>("InternalBpSlowPass");
    let _ = register_kernel::<CountingSource>("InternalBpCountingSource");
    let _ = register_kernel::<Duplicate>("InternalBpDuplicate");
    let _ = register_kernel::<Burst>("InternalBpBurst");
    let _ = register_kernel::<EmitOnClose>("InternalBpEmitOnClose");
    let _ = register_kernel::<JoinCount>("InternalBpJoinCount");
    let _ = register_kernel::<CloseJoinCount>("InternalBpCloseJoinCount");
    let _ = register_kernel::<PortJoinCount>("InternalBpPortJoinCount");
}

#[test]
fn bounded_internal_queue_preserves_all_packets() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid] }
  - name: consumer
    kernel: InternalBpSlowPass
    input_ports: [mid]
    output_ports: [out]
    executor: pool
    input_queues: { packets: 2 }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for value in 0..40 {
        input
            .send(Packet::from_i64(value).at(Timestamp(value)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();

    let values: Vec<_> =
        std::iter::from_fn(|| output.next().and_then(|packet| packet.as_i64())).collect();
    assert_eq!(values, (0..40).collect::<Vec<_>>());
    assert!(
        graph.node_stats(1).unwrap().peak_queue_depth <= 2,
        "bounded consumer queue exceeded capacity"
    );
}

#[test]
fn fast_source_is_cooperatively_paused_by_slow_sink() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: source_pool, num_threads: 1 }
  - { name: sink_pool, num_threads: 1 }
nodes:
  - name: source
    kernel: InternalBpCountingSource
    input_ports: []
    output_ports: [mid]
    executor: source_pool
  - name: sink
    kernel: InternalBpSlowPass
    input_ports: [mid]
    output_ports: [out]
    executor: sink_pool
    input_queues: { packets: 3 }
output_ports: [out]
"#,
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();
    let count = std::iter::from_fn(|| output.next()).count();
    assert_eq!(count, 100);
    assert!(graph.node_stats(1).unwrap().peak_queue_depth <= 3);
}

#[test]
fn bounded_diamond_does_not_deadlock() {
    register_test_kernels();
    JOINED.store(0, Ordering::SeqCst);
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 3 }
nodes:
  - { name: split, kernel: InternalBpDuplicate, input_ports: [in], output_ports: [left, right], executor: pool }
  - { name: slow, kernel: InternalBpSlowPass, input_ports: [left], output_ports: [slow_out], executor: pool, input_queues: { packets: 2 } }
  - { name: fast, kernel: PassThrough, input_ports: [right], output_ports: [fast_out], executor: pool, input_queues: { packets: 2 } }
  - { name: join, kernel: InternalBpJoinCount, input_ports: [slow_out, fast_out], output_ports: [], executor: pool, input_queues: { packets: 2 } }
input_ports: [in]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for value in 0..50 {
        input
            .send(Packet::from_i64(value).at(Timestamp(value)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(JOINED.load(Ordering::SeqCst), 50);
    for node in 1..4 {
        assert!(graph.node_stats(node).unwrap().peak_queue_depth <= 2);
    }
}

#[test]
fn capacity_smaller_than_one_emitted_batch_fails_loudly() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: burst, kernel: InternalBpBurst, input_ports: [in], output_ports: [mid] }
  - { name: sink, kernel: Sink, input_ports: [mid], output_ports: [], input_queues: { packets: 1 } }
input_ports: [in]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    let error = graph.wait_done().unwrap_err();
    assert!(error.to_string().contains("emits a batch of 2"));
}

#[test]
fn per_port_capacities_override_node_defaults() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: burst, kernel: InternalBpBurst, input_ports: [in], output_ports: [wide] }
  - name: join
    kernel: InternalBpPortJoinCount
    input_ports: [wide, gate]
    output_ports: []
    input_queues:
      packets: 1
      bytes: 8
      ports:
        wide: { packets: 2, bytes: 16 }
        gate: { packets: 0, bytes: 0 }
input_ports: [in, gate]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
}

#[test]
fn byte_capacity_preserves_packets_and_bounds_queue_depth() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid] }
  - name: consumer
    kernel: InternalBpSlowPass
    input_ports: [mid]
    output_ports: [out]
    executor: pool
    input_queues: { bytes: 20 }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for timestamp in 0..20 {
        input
            .send(Packet::from_bytes(vec![timestamp as u8; 10]).at(Timestamp(timestamp)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(std::iter::from_fn(|| output.next()).count(), 20);
    assert!(graph.node_stats(1).unwrap().peak_queue_depth <= 2);
}

#[test]
fn input_queue_stats_report_active_and_accumulated_backpressure() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool }
  - { name: waiting_join, kernel: InternalBpPortJoinCount, input_ports: [mid, gate], output_ports: [], executor: pool, input_queues: { packets: 1, bytes: 8 } }
input_ports: [in, gate]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::from_i64(0).at(Timestamp(0))).unwrap();
    input.send(Packet::from_i64(1).at(Timestamp(1))).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let blocked = loop {
        let stats = graph.input_queue_stats(1, 0).unwrap();
        if stats.blocked {
            break stats;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "producer never entered cooperative backpressure"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(blocked.node_name, "waiting_join");
    assert_eq!(blocked.port_name, "mid");
    assert_eq!(blocked.producer_name.as_deref(), Some("producer"));
    assert_eq!(blocked.packet_capacity, Some(1));
    assert_eq!(blocked.byte_capacity, Some(8));
    assert_eq!(blocked.queued_packets, 1);
    assert_eq!(blocked.queued_bytes, 8);
    assert_eq!(blocked.block_events, 1);
    assert!(blocked.blocked_for_us > 0);
    assert!(graph.to_dot_with_stats().contains("bp 1×"));
    assert!(graph.dump().contains("blocked=true"));

    graph
        .input("gate")
        .unwrap()
        .send(Packet::from_i64(0).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(2)).unwrap();

    let finished = graph.input_queue_stats(1, 0).unwrap();
    assert!(!finished.blocked);
    assert_eq!(finished.block_events, 1);
    assert!(finished.total_blocked_us >= blocked.blocked_for_us);
    assert_eq!(finished.peak_queued_packets, 1);
    assert_eq!(finished.peak_queued_bytes, 8);
}

#[test]
fn reset_clears_input_queue_backpressure_stats() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool }
  - { name: join, kernel: InternalBpPortJoinCount, input_ports: [mid, gate], output_ports: [], executor: pool, input_queues: { packets: 1 } }
input_ports: [in, gate]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::from_i64(0).at(Timestamp(0))).unwrap();
    input.send(Packet::from_i64(1).at(Timestamp(1))).unwrap();
    std::thread::sleep(Duration::from_millis(10));
    graph.cancel();
    let _ = graph.wait_done_timeout(Duration::from_secs(2));
    assert!(graph.input_queue_stats(1, 0).unwrap().block_events > 0);

    graph.reset().unwrap();
    let reset = graph.input_queue_stats(1, 0).unwrap();
    assert!(!reset.blocked);
    assert_eq!(reset.block_events, 0);
    assert_eq!(reset.total_blocked_us, 0);
    assert_eq!(reset.peak_queued_packets, 0);
    assert_eq!(reset.peak_queued_bytes, 0);
}

#[test]
fn max_in_flight_backpressure_stats_remain_consistent() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 4 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool, max_in_flight: 4 }
  - { name: consumer, kernel: InternalBpSlowPass, input_ports: [mid], output_ports: [out], executor: pool, input_queues: { packets: 2, bytes: 16 } }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for timestamp in 0..100 {
        input
            .send(Packet::from_i64(timestamp).at(Timestamp(timestamp)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(std::iter::from_fn(|| output.next()).count(), 100);

    let stats = graph.input_queue_stats(1, 0).unwrap();
    assert!(stats.peak_queued_packets <= 2);
    assert!(stats.peak_queued_bytes <= 16);
    assert_eq!(stats.queued_packets, 0);
    assert_eq!(stats.queued_bytes, 0);
    assert_eq!(stats.reserved_packets, 0);
    assert_eq!(stats.reserved_bytes, 0);
    assert!(!stats.blocked);
    assert!(stats.block_events > 0);
}

#[test]
fn emitted_batch_larger_than_byte_capacity_fails_loudly() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid] }
  - { name: sink, kernel: Sink, input_ports: [mid], output_ports: [], input_queues: { bytes: 8 } }
input_ports: [in]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_bytes(vec![0; 9]).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    let error = graph.wait_done().unwrap_err();
    assert!(error.to_string().contains("batch of 9 bytes"));
}

#[test]
fn byte_capacity_rejects_unmeasurable_payloads() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid] }
  - { name: sink, kernel: Sink, input_ports: [mid], output_ports: [], input_queues: { bytes: 8 } }
input_ports: [in]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::new(1u32).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    let error = graph.wait_done().unwrap_err();
    assert!(error.to_string().contains("unmeasurable payload"));
}

#[test]
fn byte_capacity_accepts_measurable_zero_length_payloads() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid] }
  - { name: sink, kernel: Sink, input_ports: [mid], output_ports: [], input_queues: { bytes: 1 } }
input_ports: [in]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_bytes(Vec::new()).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
}

#[test]
fn lossless_capacity_cannot_be_combined_with_fixed_size() {
    register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - name: sink
    kernel: Sink
    input_ports: [in]
    output_ports: []
    input_queues: { packets: 2 }
    input_policy: { type: fixed_size, capacity: 2 }
input_ports: [in]
"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("cannot be combined"));
}

#[test]
fn capacity_override_rejects_unknown_port() {
    register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - name: sink
    kernel: Sink
    input_ports: [in]
    output_ports: []
    input_queues:
      ports: { typo: { packets: 2 } }
input_ports: [in]
"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown input port `typo`"));
}

#[test]
fn legacy_split_capacity_fields_are_rejected() {
    register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - name: sink
    kernel: Sink
    input_ports: [in]
    output_ports: []
    input_queue_capacity: 2
input_ports: [in]
"#,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("unknown field `input_queue_capacity`"));
}

#[test]
fn capacity_override_rejects_back_edge_port() {
    register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - name: join
    kernel: InternalBpJoinCount
    input_ports: [in, feedback]
    output_ports: []
    back_edges: [feedback]
    input_queues:
      ports: { feedback: { bytes: 8 } }
input_ports: [in, feedback]
"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("back-edge input `feedback`"));
}

#[test]
fn cancel_releases_blocked_staging() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool }
  - { name: waiting_join, kernel: InternalBpJoinCount, input_ports: [mid, never], output_ports: [], input_queues: { packets: 1 } }
input_ports: [in, never]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::from_i64(0).at(Timestamp(0))).unwrap();
    input.send(Packet::from_i64(1).at(Timestamp(1))).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    graph.cancel();
    let error = graph.wait_done_timeout(Duration::from_secs(2)).unwrap_err();
    assert_eq!(error.code(), lmflow::status::code::CANCELLED);
}

#[test]
fn close_output_resumes_after_downstream_dequeue() {
    register_test_kernels();
    CLOSE_JOINED.store(0, Ordering::SeqCst);
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 2 }
nodes:
  - { name: producer, kernel: InternalBpEmitOnClose, input_ports: [in], output_ports: [mid], executor: pool }
  - { name: join, kernel: InternalBpCloseJoinCount, input_ports: [mid, gate], output_ports: [], executor: pool, input_queues: { packets: 1 } }
input_ports: [in, gate]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(0).at(Timestamp(0)))
        .unwrap();
    graph.close_input("in").unwrap();
    std::thread::sleep(Duration::from_millis(20));

    let gate = graph.input("gate").unwrap();
    gate.send(Packet::from_i64(0).at(Timestamp(0))).unwrap();
    gate.send(Packet::from_i64(1).at(Timestamp(1))).unwrap();
    gate.close();
    graph.wait_done_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(CLOSE_JOINED.load(Ordering::SeqCst), 2);
}

#[test]
fn impossible_alignment_reports_backpressure_stall() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid] }
  - { name: waiting_join, kernel: InternalBpJoinCount, input_ports: [mid, never], output_ports: [], input_queues: { packets: 1 } }
input_ports: [in, never]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::from_i64(0).at(Timestamp(0))).unwrap();
    input.send(Packet::from_i64(1).at(Timestamp(1))).unwrap();
    let error = graph
        .wait_until_idle_timeout(Duration::from_secs(1))
        .unwrap_err();
    assert!(error.to_string().contains("cannot make progress"));
    graph.cancel();
}
