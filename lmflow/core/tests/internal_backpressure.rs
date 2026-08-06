mod common;

use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use lmflow::{register_kernel, Graph, Kernel, KernelContract, KernelCtx, Packet, Timestamp};

static LOG_TEST_LOCK: Mutex<()> = Mutex::new(());
static BACKPRESSURE_LOGS: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

unsafe extern "C" fn capture_log(_user: *mut std::ffi::c_void, level: i32, msg: *const c_char) {
    let message = unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .into_owned();
    BACKPRESSURE_LOGS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push((level, message));
}

struct LogCallbackGuard;

impl LogCallbackGuard {
    fn install() -> Self {
        BACKPRESSURE_LOGS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        lmflow::ffi::lmflow_set_log_callback(Some(capture_log), std::ptr::null_mut());
        Self
    }
}

impl Drop for LogCallbackGuard {
    fn drop(&mut self) {
        lmflow::ffi::lmflow_set_log_callback(None, std::ptr::null_mut());
    }
}

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
    common::register_test_kernels();
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
      ports:
        wide: { packets: 2 }
        gate: { packets: 0 }
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
fn input_queue_stats_report_active_and_accumulated_backpressure() {
    let _log_guard = LOG_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _callback_guard = LogCallbackGuard::install();
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: diagnostics_producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool }
  - name: diagnostics_join
    kernel: InternalBpPortJoinCount
    input_ports: [mid, gate]
    output_ports: []
    executor: pool
    input_queues:
      packets: 1
      ports: { gate: { packets: 0 } }
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
    assert_eq!(blocked.node_name, "diagnostics_join");
    assert_eq!(blocked.port_name, "mid");
    assert_eq!(
        blocked.producer_name.as_deref(),
        Some("diagnostics_producer")
    );
    assert_eq!(blocked.packet_capacity, Some(1));
    assert_eq!(blocked.queued_packets, 1);
    assert_eq!(blocked.queued_bytes, 8);
    assert_eq!(blocked.block_events, 1);
    assert!(blocked.blocked_for_us > 0);
    assert!(graph.to_dot_with_stats().contains("bp 1×"));
    let blocked_dot = graph.to_dot_with_stats();
    assert!(blocked_dot.contains("ports:"));
    assert!(blocked_dot.contains("mid 1/1 r"));
    assert!(blocked_dot.contains("mid 1/1 r0"));
    assert!(blocked_dot.contains("HOT #"));
    assert!(blocked_dot.contains("BLOCKED: queue full"));
    assert!(blocked_dot.contains("gate 0/∞ r0"));
    assert!(blocked_dot.contains("WAITING: aligned input"));
    assert!(blocked_dot.contains("queue 1/1 · reserved"));
    assert!(blocked_dot.contains("bp 1×"));
    assert!(blocked_dot.contains("color=\"#d62728\""));
    assert!(blocked_dot.contains("BLOCKED"));
    assert!(blocked_dot.contains("WAITING: missing aligned input"));
    assert!(blocked_dot.contains("reason consumer queue full"));
    assert!(blocked_dot.contains("color=\"#d6a700\""));
    assert!(blocked_dot.contains("blocked 1 · waiting 1"));
    let blocked_durations = blocked_dot
        .match_indices("bp 1× / ")
        .map(|(index, marker)| {
            let value = &blocked_dot[index + marker.len()..];
            value
                .split(['\\', '"', ' '])
                .next()
                .expect("backpressure duration follows marker")
        })
        .collect::<Vec<_>>();
    assert!(
        blocked_durations.len() >= 2,
        "node and edge should both show the active blocked duration"
    );
    assert!(
        blocked_durations
            .iter()
            .all(|duration| duration == &blocked_durations[0]),
        "one DOT export must format every active blocked duration from the same snapshot: {blocked_durations:?}"
    );
    assert!(graph.dump().contains("capacity=1 packets"));
    assert!(graph.dump().contains("capacity=unbounded packets"));
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

    let logs = BACKPRESSURE_LOGS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let warnings: Vec<&str> = logs
        .iter()
        .filter(|(level, _)| *level == lmflow::runtime::LOG_WARN)
        .map(|(_, message)| message.as_str())
        .filter(|message| message.contains("consumer `diagnostics_join`"))
        .collect();
    assert_eq!(warnings.len(), 1, "first block should emit one warning");
    assert!(warnings[0].contains("producer `diagnostics_producer`"));
    assert!(warnings[0].contains("consumer `diagnostics_join` input `mid`"));
    assert!(warnings[0].contains("capacity=1"));
    assert!(warnings[0].contains("queued=1"));
    assert!(warnings[0].contains("incoming=1"));
    let recoveries: Vec<&str> = logs
        .iter()
        .filter(|(level, _)| *level == lmflow::runtime::LOG_INFO)
        .map(|(_, message)| message.as_str())
        .filter(|message| message.contains("consumer `diagnostics_join`"))
        .collect();
    assert_eq!(recoveries.len(), 1, "warned block should emit one recovery");
    assert!(recoveries[0].contains("producer `diagnostics_producer` resumed"));
    assert!(recoveries[0].contains("consumer `diagnostics_join` input `mid` drained"));
}

#[test]
fn dot_ranks_hotspots_and_traces_upstream_pressure_path() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: source_pass, kernel: PassThrough, input_ports: [in], output_ports: [stage1], executor: pool }
  - { name: middle_pass, kernel: PassThrough, input_ports: [stage1], output_ports: [stage2], executor: pool }
  - name: blocked_join
    kernel: InternalBpPortJoinCount
    input_ports: [stage2, gate]
    output_ports: []
    executor: pool
    input_queues:
      packets: 1
      ports: { gate: { packets: 0 } }
input_ports: [in, gate]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    input.send(Packet::from_i64(0).at(Timestamp(0))).unwrap();
    input.send(Packet::from_i64(1).at(Timestamp(1))).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let dot = loop {
        let dot = graph.to_dot_with_stats();
        if dot.contains("stage2 1/1 r0 HOT #1 BLOCKED: queue full")
            && dot.matches("PRESSURE PATH: upstream propagation").count() >= 2
        {
            break dot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "diagnostic graph never observed the blocked chain"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(dot.contains("top nodes #1 blocked_join"));
    assert!(dot.contains("top ports #1 blocked_join.stage2"));
    assert!(dot.contains("HOT #1"));
    assert!(dot.matches("PRESSURE PATH: upstream propagation").count() >= 2);
    assert!(dot.contains("color=\"#6f42c1\""));
    assert!(dot.contains("PRESSURE PATH\\nupstream propagation to active stall"));

    graph.cancel();
    graph.close_all_inputs();
    let _ = graph.wait_done_timeout(Duration::from_secs(2));
}

#[test]
fn backpressure_logs_are_exponentially_rate_limited() {
    let _log_guard = LOG_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _callback_guard = LogCallbackGuard::install();
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, num_threads: 1 }
nodes:
  - { name: rate_limit_producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool }
  - name: rate_limit_join
    kernel: InternalBpPortJoinCount
    input_ports: [mid, gate]
    output_ports: []
    executor: pool
    input_queues:
      packets: 1
      ports: { gate: { packets: 0 } }
input_ports: [in, gate]
"#,
    )
    .unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    let gate = graph.input("gate").unwrap();

    for event in 1..=3 {
        let first = (event - 1) * 2;
        input
            .send(Packet::from_i64(first).at(Timestamp(first)))
            .unwrap();
        input
            .send(Packet::from_i64(first + 1).at(Timestamp(first + 1)))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if graph.input_queue_stats(1, 0).unwrap().block_events >= event as u64 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "backpressure event #{event} was not observed"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        gate.send(Packet::from_i64(first).at(Timestamp(first)))
            .unwrap();
        gate.send(Packet::from_i64(first + 1).at(Timestamp(first + 1)))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while graph.input_queue_stats(1, 0).unwrap().blocked {
            assert!(
                std::time::Instant::now() < deadline,
                "backpressure event #{event} did not clear"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(2)).unwrap();

    let logs = BACKPRESSURE_LOGS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let backpressure: Vec<&str> = logs
        .iter()
        .map(|(_, message)| message.as_str())
        .filter(|message| message.contains("consumer `rate_limit_join`"))
        .collect();
    assert!(backpressure.iter().any(|message| message.contains("#1:")));
    assert!(backpressure.iter().any(|message| message.contains("#2:")));
    assert!(!backpressure.iter().any(|message| message.contains("#3:")));
    assert!(backpressure
        .iter()
        .any(|message| message.contains("#1 cleared")));
    assert!(backpressure
        .iter()
        .any(|message| message.contains("#2 cleared")));
    assert!(!backpressure
        .iter()
        .any(|message| message.contains("#3 cleared")));
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
  - { name: consumer, kernel: InternalBpSlowPass, input_ports: [mid], output_ports: [out], executor: pool, input_queues: { packets: 2 } }
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
    assert!(!stats.blocked);
    assert!(stats.block_events > 0);
}

#[test]
fn wait_done_does_not_mistake_claimed_pool_work_for_idle() {
    register_test_kernels();
    for round in 0..40 {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: pool, num_threads: 4 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool, max_in_flight: 4 }
  - { name: consumer, kernel: PassThrough, input_ports: [mid], output_ports: [out], executor: pool, max_in_flight: 4 }
input_ports: [in]
output_ports: [out]
"#,
        )
        .unwrap();
        let output = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for timestamp in 0..16 {
            input
                .send(Packet::from_i64(timestamp).at(Timestamp(timestamp)))
                .unwrap();
        }
        graph.close_all_inputs();
        graph
            .wait_done_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("round {round}: {error}\n{}", graph.dump()));
        assert_eq!(std::iter::from_fn(|| output.next()).count(), 16);
    }
}

#[test]
fn wait_done_does_not_mistake_executor_completion_for_stable_idle() {
    register_test_kernels();
    for round in 0..40 {
        let graph = Graph::from_yaml(
            r#"
executors:
  - { name: pool, num_threads: 4 }
nodes:
  - { name: producer, kernel: PassThrough, input_ports: [in], output_ports: [mid], executor: pool, max_in_flight: 4 }
  - { name: consumer, kernel: InternalBpSlowPass, input_ports: [mid], output_ports: [out], executor: pool, input_queues: { packets: 2 } }
input_ports: [in]
output_ports: [out]
"#,
        )
        .unwrap();
        let output = graph.add_poller("out").unwrap();
        graph.start().unwrap();
        let input = graph.input("in").unwrap();
        for timestamp in 0..32 {
            input
                .send(Packet::from_i64(timestamp).at(Timestamp(timestamp)))
                .unwrap();
        }
        graph.close_all_inputs();
        graph
            .wait_done_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("round {round}: {error}\n{}", graph.dump()));
        assert_eq!(std::iter::from_fn(|| output.next()).count(), 32);
    }
}

#[test]
fn byte_capacity_fields_are_rejected() {
    register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - { name: sink, kernel: Sink, input_ports: [in], output_ports: [], input_queues: { bytes: 8 } }
input_ports: [in]
"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown field `bytes`"));
}

#[test]
fn per_port_byte_capacity_fields_are_rejected() {
    register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes:
  - name: sink
    kernel: Sink
    input_ports: [in]
    output_ports: []
    input_queues:
      ports: { in: { bytes: 8 } }
input_ports: [in]
"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown field `bytes`"));
}

#[test]
fn max_queued_bytes_is_rejected() {
    register_test_kernels();
    let error = Graph::from_yaml(
        r#"
nodes: []
max_queued_bytes: 1024
"#,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("unknown field `max_queued_bytes`"));
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
      ports: { feedback: { packets: 8 } }
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
    let message = error.to_string();
    assert!(message.contains("cannot make progress"));
    assert!(message.contains("producer -> waiting_join.mid"));
    assert!(message.contains("capacity=1"));
    assert!(message.contains("queued=1"));
    assert!(message.contains("reserved=0"));
    assert!(message.contains("blocked="));
    graph.cancel();
}
