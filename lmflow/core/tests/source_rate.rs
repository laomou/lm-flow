//! 源节点定速(`rate`)验收 —— **纯 Rust,零 C++**。
//!
//! `rate: N` 让源每两次 `process` 至少隔 1/N 秒,算子不必自己写 sleep。
//! 用一个纯 Rust 源算子(产 count 个整数后 source_done),断言实际墙钟符合速率。

mod common;

use std::time::{Duration, Instant};

use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet, Timestamp};

/// 产 `count` 个整数(0..count)后自报产完。不自带 sleep —— 定速交给引擎。
#[derive(Default)]
struct Counter {
    count: i64,
    next: i64,
}
impl Kernel for Counter {
    fn open(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        self.count = cc.option_i64("count", 10);
        Ok(())
    }
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        if self.next >= self.count {
            cc.source_done();
            return Ok(());
        }
        cc.emit(0, Packet::from_i64(self.next))?;
        self.next += 1;
        Ok(())
    }
}

#[derive(Default)]
struct CooperativeSource {
    calls: usize,
}

impl Kernel for CooperativeSource {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        if self.calls == 0 {
            self.calls += 1;
            cc.source_yield(Duration::from_millis(200));
        } else {
            cc.source_done();
        }
        Ok(())
    }
}

#[derive(Default)]
struct InvalidYield;

impl Kernel for InvalidYield {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.source_yield(Duration::ZERO);
        cc.forward(0, 0)
    }
}

fn reg() {
    common::register_test_kernels();
    let _ = register_kernel::<Counter>("RateCounter");
    let _ = register_kernel::<CooperativeSource>("CooperativeSource");
    let _ = register_kernel::<InvalidYield>("InvalidYield");
}

fn run(rate: &str, count: i64) -> (Vec<i64>, Duration) {
    let g = Graph::from_yaml(&format!(
        "executors:\n  - {{ name: \"cpu\", type: \"ThreadPoolExecutor\", num_threads: 1 }}\n\
         nodes:\n  - {{ name: src, kernel: RateCounter, input_ports: [], output_ports: [\"out\"], executor: \"cpu\"{rate}, options: {{ count: {count} }} }}\n\
         input_ports: []\noutput_ports: [\"out\"]\n"
    ))
    .unwrap();
    let out = g.add_poller("out").unwrap();
    let t0 = Instant::now();
    g.start().unwrap();
    let mut got = Vec::new();
    while (got.len() as i64) < count {
        match out.next() {
            Some(p) => got.push(p.as_i64().unwrap()),
            None => break,
        }
    }
    g.wait_done_timeout(Duration::from_secs(10)).unwrap();
    (got, t0.elapsed())
}

/// 100Hz 产 10 个:首个不等,其余各隔 ~10ms → 总耗时约 90ms。
/// 断言用宽松下界(定速是**下界**保证,不该更快),上界给足余量避开 CI 抖动。
#[test]
fn rate_limits_source_throughput() {
    reg();
    let (got, elapsed) = run(", rate: 100.0", 10);
    assert_eq!(got, (0..10).collect::<Vec<_>>(), "值应完整有序");
    // 9 个间隔 × 10ms = 90ms 下界;首个不等,故用 8 个间隔留足余量。
    assert!(
        elapsed >= Duration::from_millis(70),
        "100Hz 产 10 个不该快于 ~70ms,实测 {elapsed:?}(定速没生效?)"
    );
    assert!(elapsed < Duration::from_secs(3), "耗时异常偏高 {elapsed:?}");
}

/// 不设 rate:应尽快产完,远快于任何限速。作为对照,证明上面的慢是 rate 造成的。
#[test]
fn no_rate_is_fast() {
    reg();
    let (got, elapsed) = run("", 10);
    assert_eq!(got.len(), 10);
    assert!(
        elapsed < Duration::from_millis(50),
        "不限速应很快,实测 {elapsed:?}"
    );
}

/// rate 用在非源节点 → 建图期报错。
#[test]
fn rate_on_non_source_rejected() {
    let err = Graph::from_yaml(
        "nodes:\n  - { name: k, kernel: PassThrough, input_ports: [\"in\"], output_ports: [\"out\"], rate: 30.0 }\ninput_ports: [\"in\"]\noutput_ports: [\"out\"]\n",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("rate only applies to source"), "{err}");
}

/// rate = NaN → 建图期报错(NaN 比较恒假,不能被放行成「不限速」)。
#[test]
fn nan_rate_rejected() {
    let err = Graph::from_yaml(
        "executors:\n  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }\nnodes:\n  - { name: s, kernel: PassThrough, input_ports: [], output_ports: [\"out\"], executor: cpu, rate: .nan }\ninput_ports: []\noutput_ports: [\"out\"]\n",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("positive, finite"), "{err}");
}

/// rate <= 0 → 建图期报错。
#[test]
fn non_positive_rate_rejected() {
    let err = Graph::from_yaml(
        "executors:\n  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }\nnodes:\n  - { name: s, kernel: PassThrough, input_ports: [], output_ports: [\"out\"], executor: cpu, rate: -1.0 }\ninput_ports: []\noutput_ports: [\"out\"]\n",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("positive, finite"), "{err}");
}

#[test]
fn cooperative_source_does_not_occupy_single_worker_while_waiting() {
    reg();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: solo, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: source, kernel: CooperativeSource, executor: solo, input_ports: [], output_ports: [] }
  - { name: relay, kernel: PassThrough, executor: solo, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    let started = Instant::now();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(7).at(Timestamp(0)))
        .unwrap();
    let packet = output
        .next_timeout(Duration::from_millis(100))
        .expect("poller should not time out")
        .expect("ordinary node should run while the source is waiting");
    assert_eq!(packet.as_i64(), Some(7));
    assert!(
        started.elapsed() < Duration::from_millis(180),
        "source_yield must release the only worker"
    );

    graph.close_all_inputs();
    graph
        .wait_done_timeout(Duration::from_secs(2))
        .expect("source should wake and finish");
}

#[test]
fn source_done_overrides_source_yield() {
    #[derive(Default)]
    struct DoneAndYield;
    impl Kernel for DoneAndYield {
        fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
            cc.source_yield(Duration::from_secs(60));
            cc.source_done();
            Ok(())
        }
    }
    let _ = register_kernel::<DoneAndYield>("DoneAndYield");
    let graph = Graph::from_yaml(
        "nodes:\n  - { name: source, kernel: DoneAndYield, input_ports: [], output_ports: [] }\n",
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .wait_done_timeout(Duration::from_millis(500))
        .expect("source_done must cancel the requested yield");
}

#[test]
fn source_yield_on_non_source_is_an_error() {
    reg();
    let graph = Graph::from_yaml(
        "nodes:\n  - { name: invalid, kernel: InvalidYield, input_ports: [in], output_ports: [out] }\ninput_ports: [in]\noutput_ports: [out]\n",
    )
    .unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    let error = graph.wait_done_timeout(Duration::from_secs(1)).unwrap_err();
    assert!(
        error.to_string().contains("only valid for source"),
        "{error}"
    );
}

#[test]
fn dot_shows_source_wait_reason_remaining_time_and_yield_count() {
    reg();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: source, kernel: CooperativeSource, input_ports: [], output_ports: [] }
"#,
    )
    .unwrap();
    graph.start().unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let dot = graph.to_dot_with_stats();
        if dot.contains("WAITING_SOURCE ·") && dot.contains("remaining · source_yield\\nyield 1×")
        {
            assert!(
                dot.contains("legend_state_waiting_source"),
                "state legend should explain WAITING_SOURCE:\n{dot}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "source never entered WAITING_SOURCE"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    graph
        .wait_done_timeout(Duration::from_secs(2))
        .expect("source should wake and finish");
}

#[test]
fn dot_distinguishes_rate_wait_from_source_yield() {
    reg();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: source, kernel: RateCounter, input_ports: [], output_ports: [out],
      rate: 5.0, options: { count: 2 } }
output_ports: [out]
"#,
    )
    .unwrap();
    let output = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    output
        .next_timeout(Duration::from_secs(1))
        .expect("source should emit its first packet");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let dot = graph.to_dot_with_stats();
        if dot.contains("WAITING_SOURCE ·") && dot.contains("remaining · rate\\nyield 0×") {
            assert!(!dot.contains("rate + source_yield"), "{dot}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rate-limited source never entered WAITING_SOURCE"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    graph
        .wait_done_timeout(Duration::from_secs(2))
        .expect("rate-limited source should finish");
}

#[test]
fn cancel_clears_a_long_source_yield_and_allows_reset() {
    #[derive(Default)]
    struct YieldOnce {
        calls: usize,
    }
    impl Kernel for YieldOnce {
        fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
            if self.calls == 0 {
                self.calls += 1;
                cc.source_yield(Duration::from_secs(60));
            } else {
                cc.source_done();
            }
            Ok(())
        }
    }
    let _ = register_kernel::<YieldOnce>("YieldOnce");
    let graph = Graph::from_yaml(
        "nodes:\n  - { name: source, kernel: YieldOnce, input_ports: [], output_ports: [] }\n",
    )
    .unwrap();

    graph.start().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    graph.cancel();
    assert!(
        matches!(
            graph.wait_done_timeout(Duration::from_millis(500)),
            Err(lmflow::Error::Cancelled)
        ),
        "cancel must not wait for the 60-second source delay"
    );

    graph.reset().unwrap();
    graph.start().unwrap();
    graph
        .wait_done_timeout(Duration::from_millis(500))
        .expect("the old delayed wake must not leak into the reset run");
}
