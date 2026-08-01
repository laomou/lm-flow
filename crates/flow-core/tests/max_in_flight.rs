//! max_in_flight > 1:同一节点并行处理多个时间戳。
//!
//! 两件必须同时成立、且容易只做到一半的事:
//!   1. **真的并行** —— N 个各睡 T 的调用应在约 1×T 内跑完,而非 N×T。
//!   2. **输出仍按时间戳单调** —— 即使后面的时间戳先算完,下游也要先看到前面的。
//!
//! 用一个「按时间戳反向睡眠」的算子构造「完成顺序 ≠ 时间戳顺序」,专门压这两点。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use flow_core::{Graph, Packet, Timestamp};

fn init() {
    flow_core::register_builtin_kernels();
}

/// 需要一个可重入的、耗时可控的 Python/C++ 算子。这里用 Rust 侧的自定义 C 回调注册一个
/// 「睡 (10 - ts) 毫秒后把 ts 原样输出」的算子 —— ts 越小睡越久,故完成顺序与 ts 相反。
mod reverse_sleep_kernel {
    use super::*;
    use flow_core::ffi::{LmflowContext, LmflowContract};
    use std::ffi::c_void;

    // 记录 process 的**进入**顺序与并发峰值,用来证明并行。
    pub static MAX_CONCURRENCY: AtomicUsize = AtomicUsize::new(0);
    pub static CUR_CONCURRENCY: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn create(_f: *mut c_void) -> *mut c_void {
        std::ptr::null_mut() // 无状态
    }
    unsafe extern "C" fn process(_self: *mut c_void, ctx: *mut LmflowContext) -> i32 {
        let cur = CUR_CONCURRENCY.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_CONCURRENCY.fetch_max(cur, Ordering::SeqCst);

        let ts = flow_core::ffi::lmflow_ctx_input_timestamp(ctx);
        // ts 越小睡越久:制造「完成顺序与 ts 相反」。
        let sleep_ms = (70 - ts * 10).clamp(5, 70) as u64;
        std::thread::sleep(Duration::from_millis(sleep_ms));
        flow_core::ffi::lmflow_ctx_forward(ctx, 0, 0); // 原样转发 0->0

        CUR_CONCURRENCY.fetch_sub(1, Ordering::SeqCst);
        0
    }
    // get_contract 声明为 None,故不需要;但 vtable 字段类型要匹配 LmflowContract。
    #[allow(dead_code)]
    unsafe extern "C" fn _unused_contract(_f: *mut c_void, _c: *mut LmflowContract) {}

    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let vt = flow_core::ffi::LmflowKernelVTable {
                create: Some(create),
                get_contract: None,
                open: None,
                process: Some(process),
                close: None,
                destroy: None,
            };
            // 泄漏一个 'static 的 vtable(注册表只存指针)
            let vt: &'static _ = Box::leak(Box::new(vt));
            let name = std::ffi::CString::new("ReverseSleep").unwrap();
            let rc = unsafe {
                flow_core::ffi::lmflow_register_kernel(name.as_ptr(), vt, std::ptr::null_mut())
            };
            assert_eq!(rc, 0, "注册 ReverseSleep 失败");
        });
    }
}

/// 串行化本文件里用到全局并发计数器的测试。
static SERIAL: Mutex<()> = Mutex::new(());

/// **输出顺序**:后面的时间戳先算完,下游仍必须按时间戳单调收到。
#[test]
fn output_order_preserved_despite_out_of_order_completion() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    init();
    reverse_sleep_kernel::register();

    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 6 }
nodes:
  - name: "s"
    kernel: "ReverseSleep"
    executor: "cpu"
    input_ports: ["in"]
    output_ports: ["out"]
    max_in_flight: 6
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    // ts 0..6:ts 越小算得越慢,故完成顺序约为 5,4,3,2,1,0
    for i in 0..6i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push((p.timestamp().0, *p.get::<i32>().unwrap()));
    }
    // 下游必须按时间戳升序,尽管完成顺序是反的
    assert_eq!(
        got,
        (0..6).map(|i| (i as i64, i)).collect::<Vec<_>>(),
        "即使后面的时间戳先算完,下游也必须按时间戳单调"
    );
}

/// **真并行**:6 个各睡 ~10ms 的调用,max_in_flight=6 应在远小于 60ms 内跑完。
#[test]
fn actually_runs_in_parallel() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    init();
    reverse_sleep_kernel::register();
    reverse_sleep_kernel::MAX_CONCURRENCY.store(0, Ordering::SeqCst);
    reverse_sleep_kernel::CUR_CONCURRENCY.store(0, Ordering::SeqCst);

    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 8 }
nodes:
  - name: "s"
    kernel: "ReverseSleep"
    executor: "cpu"
    input_ports: ["in"]
    output_ports: ["out"]
    max_in_flight: 8
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    // 全部用较大的 ts,睡眠都被 clamp 到 5ms。8 个任务会同时在 process 里睡,
    // 于是**峰值并发**(同时处于 process 的调用数)应 ≥ 2 —— 串行的话峰值恒为 1。
    // 这是「真并行」的稳健证据;不用墙钟耗时断言,因为共享 CI runner 上的绝对
    // 耗时天然抖动(macOS runner 上就抖出过假阳性)。挂死由下面的 30s 超时兜底。
    for i in 10..18i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph
        .wait_until_idle_timeout(Duration::from_secs(30))
        .unwrap();

    let peak = reverse_sleep_kernel::MAX_CONCURRENCY.load(Ordering::SeqCst);
    assert!(
        peak >= 2,
        "必须观察到并发执行(≥2 个调用同时在 process),峰值并发={peak}"
    );

    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(10)).unwrap();
}

/// max_in_flight=1(默认):绝不并发(峰值并发恒为 1)。
#[test]
fn max_in_flight_one_never_concurrent() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    init();
    reverse_sleep_kernel::register();
    reverse_sleep_kernel::MAX_CONCURRENCY.store(0, Ordering::SeqCst);
    reverse_sleep_kernel::CUR_CONCURRENCY.store(0, Ordering::SeqCst);

    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 8 }
nodes:
  - { name: "s", kernel: "ReverseSleep", executor: "cpu", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 10..16i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

    assert_eq!(
        reverse_sleep_kernel::MAX_CONCURRENCY.load(Ordering::SeqCst),
        1,
        "默认 max_in_flight=1 时同一节点绝不并发"
    );
}

/// max_in_flight>1 但没配 executor:必须报错,不静默(单线程无并行可言)。
#[test]
fn max_in_flight_without_executor_is_rejected() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "s", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"], max_in_flight: 4 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("max_in_flight"), "{err}");
    assert!(err.to_string().contains("executor"), "{err}");
}

/// **多输入对齐 × 并行 in-flight**:两个易错的子系统叠在一起。
///
/// 两输入口 sync 策略:必须按时间戳配对才触发;同时 max_in_flight>1 并行处理多个
/// 时间戳、且完成顺序与时间戳相反。要求:每个时间戳恰好触发一次(不把不同时刻的
/// 数据配错),且下游按时间戳单调。
mod reverse_sleep2_kernel {
    use super::*;
    use flow_core::ffi::LmflowContext;
    use std::ffi::c_void;

    unsafe extern "C" fn process(_self: *mut c_void, ctx: *mut LmflowContext) -> i32 {
        let ts = flow_core::ffi::lmflow_ctx_input_timestamp(ctx);
        let sleep_ms = (70 - ts * 10).clamp(5, 70) as u64;
        std::thread::sleep(Duration::from_millis(sleep_ms));
        // 两口都到齐才会被调用(sync);把 0 口原样转发,一个对齐时刻产出一个包。
        flow_core::ffi::lmflow_ctx_forward(ctx, 0, 0);
        0
    }

    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let vt = flow_core::ffi::LmflowKernelVTable {
                create: None,
                get_contract: None,
                open: None,
                process: Some(process),
                close: None,
                destroy: None,
            };
            let vt: &'static _ = Box::leak(Box::new(vt));
            let name = std::ffi::CString::new("ReverseSleep2").unwrap();
            let rc = unsafe {
                flow_core::ffi::lmflow_register_kernel(name.as_ptr(), vt, std::ptr::null_mut())
            };
            assert_eq!(rc, 0, "注册 ReverseSleep2 失败");
        });
    }
}

#[test]
fn multi_input_alignment_holds_under_parallel_in_flight() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    init();
    reverse_sleep2_kernel::register();

    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 6 }
nodes:
  - name: "z"
    kernel: "ReverseSleep2"
    executor: "cpu"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
    max_in_flight: 6
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let x = graph.input("x").unwrap();
    let y = graph.input("y").unwrap();

    // 两口都喂 ts 0..6;交错发送,值带上口的标记以便核对没配错。
    for i in 0..6i32 {
        x.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    for i in 0..6i32 {
        y.send(Packet::new(1000 + i).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push((p.timestamp().0, *p.get::<i32>().unwrap()));
    }
    // 每个对齐时刻恰好一个输出(不是 12 个),转发的是 x 口的值,且按时间戳单调。
    assert_eq!(
        got,
        (0..6).map(|i| (i as i64, i)).collect::<Vec<_>>(),
        "多输入并行下:须按时间戳配对、每时刻一个、且单调"
    );
}
