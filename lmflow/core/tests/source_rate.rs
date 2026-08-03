//! 源节点定速(`rate`)验收 —— **纯 Rust,零 C++**。
//!
//! `rate: N` 让源每两次 `process` 至少隔 1/N 秒,算子不必自己写 sleep。
//! 用一个纯 Rust 源算子(产 count 个整数后 source_done),断言实际墙钟符合速率。

use std::time::{Duration, Instant};

use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet};

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

fn reg() {
    let _ = register_kernel::<Counter>("RateCounter");
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
