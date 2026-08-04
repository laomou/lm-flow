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
//!
//! ⚠ **本文件的测试必须串行跑**(`--test-threads=1`)。`rss_kb()` 读的是 `/proc/self/status`,
//! 即**整个进程**的 RSS —— 两条测量型测试并行时会互相污染(实测:单跑增长 4364 KiB,
//! 与另一条并行时虚高到 8720 KiB,多出来的是对方的常驻 footprint 而非泄漏)。
//! CI 的 soak 步骤已带 `--test-threads=1`。

use std::time::Duration;

use lmflow::{register_kernel, BufferData, Builtin, Graph, Kernel, KernelCtx, Packet, Timestamp};

/// 每包 payload 大小。要足够大,才能让「泄漏」与「常数开销」在 RSS 上区分得开。
const PACKET_BYTES: usize = 128 * 1024;
/// 全局字节水位。128 KiB × 32 = 4 MiB,故队列最多积 ~32 个包。
const MAX_QUEUED_BYTES: u64 = 4 * 1024 * 1024;
/// 默认包数(CI 友好:约 250 MiB 吞吐、数秒)。可用环境变量放大。
const DEFAULT_PACKETS: usize = 2000;
/// diamond 用**按个数**的水位(字节水位对 Native/Foreign payload 计 0,只对内建有效)。
/// 注意扇出后每包占 **2 个**队列槽 —— `on_enqueue` 是按消费者调用的。
const DIAMOND_MAX_QUEUED_PACKETS: usize = 64;

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

/// 慢转发者:取走、睡一会、原样产出 —— diamond 里的慢分支。
#[derive(Default)]
struct SlowRelay;
impl Kernel for SlowRelay {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let pkt = cc.take_input(0);
        std::thread::sleep(Duration::from_micros(200));
        cc.emit(0, pkt)
    }
}

/// 快转发者:立刻产出 —— diamond 里的快分支。
#[derive(Default)]
struct FastRelay;
impl Kernel for FastRelay {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.forward(0, 0)
    }
}

/// 汇合点:两路都取走、都释放,不产出(sink)。默认 `sync` 策略 → 按时间戳对齐。
#[derive(Default)]
struct JoinSink {
    seen: u64,
}
impl Kernel for JoinSink {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let _a = cc.take_input(0);
        let _b = cc.take_input(1);
        self.seen += 1;
        cc.counter_add("join.fired", 1);
        Ok(())
    }
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

/// **扇出 + 汇合(diamond)拓扑下的内存曲线** —— ADR #11 / §7.5 唯一要保护的那个形状。
///
/// ```text
///            ┌─► slow ─► s ─┐
///   in ──────┤               ├──► join(sync 对齐)
///            └─► fast ─► f ─┘
/// ```
///
/// 为什么必须单独测:`docs/design.md` §13.1 一直声称有一条「专门构造扇出汇合(diamond)
/// 拓扑 + 慢分支」的死锁回归 —— **实际并不存在**。全仓库每一个多输入节点都是从**图输入口**
/// 喂的,没有任何一处是内部边扇出后再汇合。也就是说 ADR #11「内部边不设硬上界」的论证
/// 本身此前是**断言而非实测**,而唯一的内存证据(上一条 soak)是单节点线性图。
///
/// 本测试同时回答三个问题:
///   1. **活性** —— 慢分支拖着,图仍能跑完(内部边无界 ⇒ 不形成循环等待);
///   2. **内存** —— RSS 增长仍受**水位**约束,而非受总吞吐约束(与线性图同一结论)。
///   3. **内部背压统计** —— join 每口的字节峰值不越界,且确实记录到阻塞事件。
///
/// 用 `max_queued_packets`(按**个数**)而非字节水位:`Payload::byte_size()` 对
/// `Native` / `Foreign` 计 0,故字节水位只对内建 payload 有效,而个数水位对所有形态都成立
/// (`flow.h` 已注明这点)。这里用 Buffer payload,两种水位都能生效,但**断言挂在个数上**。
#[test]
#[ignore = "长跑压测:默认不进常规套件,用 --ignored 显式跑"]
fn watermark_bounds_memory_in_diamond_topology() {
    let _ = register_kernel::<SlowRelay>("SoakSlowRelay");
    let _ = register_kernel::<FastRelay>("SoakFastRelay");
    let _ = register_kernel::<JoinSink>("SoakJoinSink");
    let n = packets();

    // 一条图输入边被**两个**节点消费 = 真正的内部扇出;两路再汇到 join 的两个输入口。
    // join 用默认 sync 策略:必须两路在同一时间戳齐备才触发 —— 这正是 §7.5 里
    // 「D 要等 B 那一路」的那个 D。
    // 水位按个数:扇出后每包占 2 个队列槽(on_enqueue 是**每消费者**一次)。
    let cfg = format!(
        r#"
executors:
  - {{ name: "cpu", type: "ThreadPoolExecutor", num_threads: 3 }}
nodes:
  - {{ name: "slow", kernel: "SoakSlowRelay", executor: "cpu", input_ports: ["in"], output_ports: ["s"] }}
  - {{ name: "fast", kernel: "SoakFastRelay", executor: "cpu", input_ports: ["in"], output_ports: ["f"] }}
  - name: "join"
    kernel: "SoakJoinSink"
    executor: "cpu"
    input_ports: ["s", "f"]
    output_ports: []
    input_queues:
      packets: 2
      bytes: {internal_bytes}
input_ports: ["in"]
max_queue_size: 100000
max_queued_packets: {DIAMOND_MAX_QUEUED_PACKETS}
"#,
        internal_bytes = 2 * PACKET_BYTES,
    );

    let graph = Graph::from_yaml(&cfg).unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    let shared = graph.shared_for_inspection();

    for i in 0..32 {
        input.send(big_packet(i)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(50));
    let baseline = rss_kb();

    let mut curve: Vec<u64> = Vec::new();
    let mut peak_queued: usize = 0;
    let sample_every = (n / 20).max(1);

    for i in 32..n {
        input
            .send(big_packet(i))
            .unwrap_or_else(|e| panic!("send #{i} failed: {e}"));
        peak_queued = peak_queued.max(shared.total_queued());
        if i % sample_every == 0 {
            if let Some(kb) = rss_kb() {
                curve.push(kb);
            }
        }
    }

    graph.close_all_inputs();
    // 活性断言:慢分支拖着,但内部边无界 ⇒ 不该形成循环等待。若哪天给内部边加了
    // 阻塞式硬上界,这里就是第一个挂住的地方 —— 那正是 ADR #11 要防的东西。
    graph
        .wait_done_timeout(Duration::from_secs(300))
        .expect("diamond 必须能跑完 —— 挂住就说明扇出汇合形成了循环等待");

    let pushed_mib = (n as u64 * PACKET_BYTES as u64) / (1024 * 1024);
    let fired = graph.counter_value("join.fired");
    println!("diamond: {n} packets × {PACKET_BYTES} B = {pushed_mib} MiB pushed");
    println!("diamond: join 触发 {fired} 次(每次消费两路各一包)");
    println!("diamond: peak total_queued = {peak_queued} 槽(水位 64)");
    let mut total_block_events = 0u64;
    for port in 0..2 {
        let stats = graph.input_queue_stats(2, port).unwrap();
        println!(
            "diamond: join.{} peak={}/{}B blocks={} total={}us",
            stats.port_name,
            stats.peak_queued_packets,
            stats.peak_queued_bytes,
            stats.block_events,
            stats.total_blocked_us,
        );
        assert!(stats.peak_queued_packets <= 2);
        assert!(stats.peak_queued_bytes <= (2 * PACKET_BYTES) as u64);
        total_block_events = total_block_events.saturating_add(stats.block_events);
    }
    assert!(
        total_block_events > 0,
        "长跑慢分支 diamond 应让 join 的至少一个输入口触发内部背压"
    );

    // 汇合点必须把每个时间戳都对齐处理掉 —— 无丢失、无错配。
    assert_eq!(
        fired, n as i64,
        "sync 汇合应对齐处理全部 {n} 个时间戳,实际 {fired}"
    );
    // 水位真的被压到过(否则内存断言不具说服力)。
    assert!(
        peak_queued >= DIAMOND_MAX_QUEUED_PACKETS / 2,
        "水位从未被压到(峰值 {peak_queued} 槽 < 半个水位)—— 本次没形成积压,断言无意义"
    );

    let Some(base) = baseline else {
        println!("diamond: 非 Linux,跳过 RSS 断言(活性与无丢失断言仍有效)");
        return;
    };
    let peak_rss = curve.iter().copied().max().unwrap_or(base);
    let growth_kb = peak_rss.saturating_sub(base);
    println!("diamond: RSS 基线 {base} KiB → 峰值 {peak_rss} KiB(增长 {growth_kb} KiB)");
    println!("diamond: RSS 曲线(KiB){curve:?}");

    // 64 槽 × 128 KiB = 8 MiB 的在途上界,加 32 MiB 常数开销(分配器 / 三个线程栈 /
    // 扇出时同一 payload 被两路共享故实际更省)。关键仍是**该界与 n 无关**。
    let allowed_kb = DIAMOND_MAX_QUEUED_PACKETS as u64 * (PACKET_BYTES as u64 / 1024) + 32 * 1024;
    assert!(
        growth_kb <= allowed_kb,
        "diamond 下 RSS 增长 {growth_kb} KiB 超过允许的 {allowed_kb} KiB。\
         本次推入 {pushed_mib} MiB —— 若增长与吞吐同量级,说明扇出/汇合路径上有包没被释放"
    );
}
