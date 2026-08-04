//! 全局水位的长跑内存曲线压测(§15.2 / §15.3 点名的缺口)。
//!
//! 此前全局水位**只有功能测试**:证明了「撞到水位会转成背压」,但没人验证过
//! **真实内存曲线**是平的 —— 而「功能对、内存仍在涨」是完全可能的(比如某条路径
//! 漏了一次 `on_dequeue`,水位读数正常而实际占用单调增长)。
//!
//! 本文件的核心断言不是「内存小」,而是:
//!
//!   **RSS 的增长被水位约束,而不是被总吞吐量约束。**
//!
//! 全程推 `PACKETS × 128 KiB` 的数据(默认 250 MiB)穿过一个 4 MiB 的字节水位。
//! 若有泄漏,RSS 会跟着**总吞吐**涨(几百 MiB);若水位真的有效,RSS 只会在
//! 「基线 + 水位 + 常数开销」附近平掉。两者相差两个数量级,信号不可能看错。
//!
//! 默认 `#[ignore]` —— 长跑不该拖慢常规 `cargo test`。跑法:
//!
//! ```text
//! cargo test --test soak -- --ignored --nocapture          # 默认规模,几秒
//! LMFLOW_SOAK_PACKETS=20000 cargo test --test soak -- --ignored --nocapture
//! ```
//!
//! `--nocapture` 会打印采样到的 RSS 曲线,便于人眼看趋势(自动断言之外的补充)。
//!
//! 纯 Rust 算子,故 `--no-default-features` 下同样可跑。

use std::time::Duration;

use lmflow::{register_kernel, BufferData, Builtin, Graph, Kernel, KernelCtx, Packet, Timestamp};

/// 每包 payload 大小。要足够大,才能让「泄漏」与「常数开销」在 RSS 上区分得开。
const PACKET_BYTES: usize = 128 * 1024;
/// 全局字节水位。128 KiB × 32 = 4 MiB,故队列最多积 ~32 个包。
const MAX_QUEUED_BYTES: u64 = 4 * 1024 * 1024;
/// 默认包数(CI 友好:约 250 MiB 吞吐、数秒)。可用环境变量放大。
const DEFAULT_PACKETS: usize = 2000;

fn packets() -> usize {
    std::env::var("LMFLOW_SOAK_PACKETS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PACKETS)
}

/// 当前进程 RSS(KiB)。Linux 专属 —— 读 `/proc/self/status` 的 `VmRSS`,
/// 单位本来就是 KiB,故不必知道页大小(也就不必依赖 libc 去问 `sysconf`)。
fn rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        s.lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|n| n.parse().ok())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 慢消费者:取走输入立刻释放,再睡一会 —— 让生产端必然跑在前面、把队列压到水位。
#[derive(Default)]
struct SlowConsumer;

impl Kernel for SlowConsumer {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        // 取走 = 所有权移交本函数,离开作用域即释放。若这里改成 `cc.input(0)`
        // (只借用),包会留在 Context 里到下次 reset —— 那正是曾经真实发生过的
        // 「输入槽残留引用」缺陷(见 §13.2),本测试也能抓到它。
        let _pkt = cc.take_input(0);
        std::thread::sleep(Duration::from_micros(150));
        Ok(())
    }
}

fn big_packet(i: usize) -> Packet {
    let mut bd = BufferData::new(&[PACKET_BYTES as i64], 0 /* u8 */).unwrap();
    bd.bytes = vec![(i & 0xFF) as u8; PACKET_BYTES];
    Packet::from_builtin(Builtin::Buffer(bd)).at(Timestamp(i as i64))
}

#[test]
#[ignore = "长跑压测:默认不进常规套件,用 --ignored 显式跑"]
fn watermark_bounds_memory_over_long_run() {
    let _ = register_kernel::<SlowConsumer>("SoakSlowConsumer");
    let n = packets();

    // 线程池是必需的:没有 executor 时节点跑在宿主线程、`send` 期间被 pump,
    // 根本形成不了积压,水位也就永远撞不到 —— 那样测试会「通过」但什么也没证明。
    let cfg = format!(
        r#"
executors:
  - {{ name: "cpu", type: "ThreadPoolExecutor", num_threads: 1 }}
nodes:
  - {{ name: "c", kernel: "SoakSlowConsumer", executor: "cpu", input_ports: ["in"], output_ports: [] }}
input_ports: ["in"]
max_queue_size: 100000
max_queued_bytes: {MAX_QUEUED_BYTES}
"#
    );

    let graph = Graph::from_yaml(&cfg).unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    let shared = graph.shared_for_inspection();

    // 预热后再取基线:此时线程池已起、分配器已暖,基线才有意义。
    for i in 0..32 {
        input.send(big_packet(i)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(50));
    let baseline = rss_kb();

    let mut curve: Vec<u64> = Vec::new();
    let mut peak_queued_bytes: u64 = 0;
    let sample_every = (n / 20).max(1);

    for i in 32..n {
        input
            .send(big_packet(i))
            .unwrap_or_else(|e| panic!("send #{i} failed: {e}"));

        peak_queued_bytes = peak_queued_bytes.max(shared.total_queued_bytes());
        if i % sample_every == 0 {
            if let Some(kb) = rss_kb() {
                curve.push(kb);
            }
        }
    }

    graph.close_all_inputs();
    graph
        .wait_done_timeout(Duration::from_secs(300))
        .expect("soak run should terminate");

    let pushed_mib = (n as u64 * PACKET_BYTES as u64) / (1024 * 1024);
    println!("soak: {n} packets × {PACKET_BYTES} B = {pushed_mib} MiB pushed");
    println!(
        "soak: peak total_queued_bytes = {} KiB (watermark {} KiB)",
        peak_queued_bytes / 1024,
        MAX_QUEUED_BYTES / 1024
    );

    // ---- 断言 1:水位真的被撞到了 ----
    // 没有这条,一个「太快跑完、从不积压」的测试也会绿 —— 那就什么都没证明。
    assert!(
        peak_queued_bytes >= MAX_QUEUED_BYTES / 2,
        "水位从未被真正压到(峰值 {peak_queued_bytes} B < 半个水位)—— \
         本次运行没有形成积压,故内存断言不具说服力;请增大 LMFLOW_SOAK_PACKETS \
         或调慢消费者"
    );

    // ---- 断言 2:软水位的超出量有界 ----
    // 水位是 Relaxed 快照读的**软阈值**,允许滞后;但滞后应是「几个包」的量级,
    // 不该是无界的。给 8 个包的宽容度。
    let slack = 8 * PACKET_BYTES as u64;
    assert!(
        peak_queued_bytes <= MAX_QUEUED_BYTES + slack,
        "积压字节峰值 {} KiB 超出水位 {} KiB 太多(宽容 {} KiB)—— 背压没有真正生效",
        peak_queued_bytes / 1024,
        MAX_QUEUED_BYTES / 1024,
        slack / 1024
    );

    // ---- 断言 3:所有包都被处理,无丢失 ----
    let processed = graph.node_stats(0).unwrap().processed;
    assert_eq!(processed, n as u64, "背压路径不该丢包");

    // ---- 断言 4(核心):RSS 增长受水位约束,而非受总吞吐约束 ----
    let Some(base) = baseline else {
        println!("soak: 非 Linux 平台,跳过 RSS 曲线断言(水位与丢包断言仍然有效)");
        return;
    };
    let peak_rss = curve.iter().copied().max().unwrap_or(base);
    let growth_kb = peak_rss.saturating_sub(base);

    println!("soak: RSS 基线 {base} KiB → 峰值 {peak_rss} KiB(增长 {growth_kb} KiB)");
    println!("soak: RSS 曲线(KiB){curve:?}");

    // 允许「水位 + 32 MiB」的常数开销(分配器碎片、线程栈、测试自身)。
    // 关键在于这个界与 `n` **无关**:真有泄漏时增长会跟着 pushed_mib 走
    // (默认规模 250 MiB),与这里的上界差两个数量级,不会误判。
    let allowed_kb = MAX_QUEUED_BYTES / 1024 + 32 * 1024;
    assert!(
        growth_kb <= allowed_kb,
        "RSS 增长 {growth_kb} KiB 超过允许的 {allowed_kb} KiB。\
         本次共推入 {pushed_mib} MiB —— 若增长与吞吐同量级,说明包没有被释放\
         (水位读数可能仍然正常:那意味着某条路径漏了 on_dequeue 之外的释放)"
    );
}
