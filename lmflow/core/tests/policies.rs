//! 输入策略测试(docs/design.md §7.10)。
//!
//! 三种策略的差别只在两处:**就绪条件**与**入队时是否丢包**。
//! 这里各自验证到,并特别确认 `fixed_size` 的丢包**绝不静默**。

#![cfg(feature = "builtin-kernels")] // 用内置 C++ 算子:纯 Rust 构建(--no-default-features)时整文件跳过

use std::time::Duration;

use lmflow::{Graph, Packet, Timestamp};

fn init() {
    lmflow::register_builtin_kernels();
}

/// `sync`(默认):所有输入口齐备才触发。缺一路就不动。
#[test]
fn sync_waits_for_all_inputs() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "z"
    kernel: "ZipKernel"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    // 只喂 A 口:不该产出
    graph
        .input("x")
        .unwrap()
        .send(Packet::from_i64(1i32 as i64).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();
    assert!(
        poller.try_next().is_none(),
        "sync should not fire when one input is missing"
    );

    // 补上 B 口:应产出 1+2=3
    graph
        .input("y")
        .unwrap()
        .send(Packet::from_i64(2i32 as i64).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();
    let out = poller
        .try_next()
        .expect("should produce once all inputs present");
    assert_eq!(out.as_i64(), Some(3), "1 + 2 = 3");

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// `immediate`:任一口有数据就触发,不等其它口。
#[test]
fn immediate_fires_on_any_input() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "z"
    kernel: "ZipKernel"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
    input_policy: { type: "immediate" }
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();

    // 只喂一路 —— immediate 下 Process 会被调用(ZipKernel 自己判断缺一路则不产出)
    graph
        .input("x")
        .unwrap()
        .send(Packet::from_i64(1i32 as i64).at(Timestamp(0)))
        .unwrap();
    graph.wait_until_idle().unwrap();

    let st = graph.node_stats(0).unwrap();
    assert_eq!(
        st.processed, 1,
        "under immediate policy a single input should still trigger Process (sync would not)"
    );

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// `fixed_size`:队列满则丢最旧的包,且丢包必须可观测。
#[test]
fn fixed_size_drops_oldest_and_reports_it() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "p"
    kernel: "PassThroughKernel"
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: "fixed_size", capacity: 2 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    // 暂停调度,隔离 fixed_size 行为 —— 否则空闲节点会立刻认领第一个包(把它从队列取走),
    // 那 1 个「在飞」的包就不受 fixed_size 丢弃约束了。暂停下所有包都留在队列里,
    // 于是能纯粹验证「满则丢最旧」。
    graph.pause();
    for i in 0..10i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    assert_eq!(
        graph.queue_depth("in"),
        Some(2),
        "queue must not exceed capacity"
    );
    assert_eq!(
        graph.dropped_count("in"),
        Some(8),
        "dropped count must be accounted for (never silent)"
    );

    graph.resume();
    graph.wait_until_idle().unwrap();
    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(*p.get::<i32>().unwrap());
    }
    // 丢的是**最旧**的,留下的是最新两个
    assert_eq!(
        got,
        vec![8, 9],
        "should keep the newest packets, drop the oldest"
    );

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// `fixed_size` 与线程池并用:实时管线的典型配置。
#[test]
fn fixed_size_with_pool_bounds_memory() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "cpu", type: "ThreadPoolExecutor", num_threads: 2 }
nodes:
  - name: "p"
    kernel: "PassThroughKernel"
    executor: "cpu"
    input_ports: ["in"]
    output_ports: ["out"]
    input_policy: { type: "fixed_size", capacity: 4 }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();

    for i in 0..500i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    // 无论消费快慢,队列都不会超过 capacity —— 这正是它存在的意义
    assert!(
        graph.queue_depth("in").unwrap() <= 4,
        "fixed_size must pin the queue within capacity"
    );

    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();
}

/// 绑核的线程池:配了 affinity 的图必须照常正确跑完(绑核是优化,不能改变结果)。
#[test]
fn pool_with_affinity_runs_correctly() {
    init();
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: "rt", type: "ThreadPoolExecutor", num_threads: 2, affinity: [0, 1] }
nodes:
  - name: "p"
    kernel: "PassThroughKernel"
    executor: "rt"
    input_ports: ["in"]
    output_ports: ["out"]
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let input = graph.input("in").unwrap();
    for i in 0..20i32 {
        input.send(Packet::new(i).at(Timestamp(i as i64))).unwrap();
    }
    graph.close_all_inputs();
    graph.wait_done_timeout(Duration::from_secs(30)).unwrap();

    let mut got = Vec::new();
    while let Some(p) = poller.try_next() {
        got.push(*p.get::<i32>().unwrap());
    }
    assert_eq!(
        got,
        (0..20).collect::<Vec<_>>(),
        "CPU pinning must not change the processing result"
    );
}

/// `fixed_size` + **多输入**:每口队列各自有界。丢包会在某口留下时间戳缺口,
/// sync 对齐靠 bound 推进跳过它 —— 关键是**不死锁**、丢包可观测、能正常终结。
/// (单输入的 fixed_size 已另测;多输入是对齐 × 丢弃的叠加,最容易出死锁。)
#[test]
fn fixed_size_multi_input_does_not_deadlock() {
    init();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "z"
    kernel: "ZipKernel"
    input_ports: ["A:x", "B:y"]
    output_ports: ["out"]
    input_policy: { type: "fixed_size", capacity: 2 }
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let x = graph.input("x").unwrap();
    let y = graph.input("y").unwrap();
    // x 口猛灌(必溢出丢弃),y 口少量;时间戳交错。用内建 I64 以匹配 ZipKernel 契约。
    for i in 0..10i32 {
        x.send(Packet::from_i64(i as i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    for i in 0..4i32 {
        y.send(Packet::from_i64((i * 100) as i64).at(Timestamp(i as i64)))
            .unwrap();
    }
    graph.close_all_inputs();
    // 必须能终结(不死锁);30s 兜底超时会把死锁暴露成失败而非永久挂住
    graph
        .wait_done_timeout(Duration::from_secs(30))
        .expect("fixed_size multi-input must terminate normally, no deadlock");
    assert_eq!(
        graph.dropped_count("x"),
        Some(8),
        "port x should drop the oldest 8, observably"
    );
}

#[test]
fn fixed_size_rejects_zero_capacity() {
    init();
    // capacity 0 意味着每个包都丢,几乎肯定是漏配
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"],
      input_policy: { type: "fixed_size", capacity: 0 } }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("capacity"), "{err}");
}

#[test]
fn rejects_unknown_policy() {
    init();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: "p", kernel: "PassThroughKernel", input_ports: ["in"], output_ports: ["out"],
      input_policy: { type: "nonsense" } }
input_ports: ["in"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown input_policy"), "{err}");
}

/// 测试用算子:每次触发时,把「哪些输入口非空」编码成位掩码(bit i = 输入口 i 有包)发出。
/// 用它验证 SyncSet 每次只带上就绪那组的口。
mod port_probe {
    use lmflow::ffi::*;
    use std::ffi::c_void;

    unsafe extern "C" fn process(_s: *mut c_void, ctx: *mut LMFlowContext) -> i32 {
        let n = lmflow_ctx_num_inputs(ctx);
        let mut mask: i64 = 0;
        for i in 0..n {
            if !lmflow_ctx_input_is_empty(ctx, i) {
                mask |= 1i64 << i;
            }
        }
        let ts = lmflow_ctx_input_timestamp(ctx);
        lmflow_ctx_emit(ctx, 0, lmflow_packet_from_i64(mask, ts));
        0
    }

    pub fn register() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let vt = LMFlowKernelVTable {
                create: None,
                get_contract: None,
                open: None,
                process: Some(process),
                close: None,
                destroy: None,
            };
            // 引擎在 register 内按值拷贝 vtable,返回后不再引用 —— 故栈上量即可,无需泄漏。
            let name = std::ffi::CString::new("PortProbe").unwrap();
            let rc = unsafe { lmflow_register_kernel(name.as_ptr(), &vt, std::ptr::null_mut()) };
            assert_eq!(rc, 0, "failed to register PortProbe");
        });
    }
}

/// SyncSet:分组各自按时间戳对齐、独立触发,每次只带**就绪那组**的口。
#[test]
fn sync_set_fires_groups_independently() {
    init();
    port_probe::register();
    // 三个输入口 x=0,y=1,z=2;分成 {x,y} 与 {z}
    let graph = Graph::from_yaml(
        r#"
nodes:
  - name: "n"
    kernel: "PortProbe"
    input_ports: ["x", "y", "z"]
    output_ports: ["out"]
    input_policy: { type: "sync_set", sets: [["x", "y"], ["z"]] }
input_ports: ["x", "y", "z"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    let (x, y, z) = (
        graph.input("x").unwrap(),
        graph.input("y").unwrap(),
        graph.input("z").unwrap(),
    );

    // 只喂 x:{x,y} 组不齐,不该触发
    x.send(Packet::from_i64(1).at(Timestamp(0))).unwrap();
    graph.wait_until_idle().unwrap();
    assert!(
        out.try_next().is_none(),
        "{{x,y}} group should not fire while missing y"
    );

    // 补上 y@0:{x,y} 组齐 → 触发一次,掩码含 x、y(bit0|bit1=3),不含 z
    y.send(Packet::from_i64(2).at(Timestamp(0))).unwrap();
    graph.wait_until_idle().unwrap();
    let p = out.try_next().expect("{{x,y}} should fire once complete");
    assert_eq!(p.timestamp().0, 0);
    assert_eq!(p.as_i64(), Some(0b011), "should carry only x and y, not z");
    assert!(out.try_next().is_none(), "should have no extra output");

    // 喂 z@5:{z} 组独立触发 → 掩码只含 z(bit2=4)
    z.send(Packet::from_i64(9).at(Timestamp(5))).unwrap();
    graph.wait_until_idle().unwrap();
    let p = out
        .try_next()
        .expect("{{z}} group should fire independently");
    assert_eq!(p.timestamp().0, 5);
    assert_eq!(p.as_i64(), Some(0b100), "should carry only z");

    graph.close_all_inputs();
    let _ = graph.wait_done();
}

/// SyncSet 配置校验:分组必须覆盖全部输入口(否则建图报错)。
#[test]
fn sync_set_rejects_incomplete_partition() {
    init();
    port_probe::register();
    let err = Graph::from_yaml(
        r#"
nodes:
  - name: "n"
    kernel: "PortProbe"
    input_ports: ["x", "y", "z"]
    output_ports: ["out"]
    input_policy: { type: "sync_set", sets: [["x", "y"]] }
input_ports: ["x", "y", "z"]
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("cover all input ports"), "{err}");
}

/// SyncSet 配置校验:引用不存在的端口名 → 报错。
#[test]
fn sync_set_rejects_unknown_port() {
    init();
    port_probe::register();
    let err = Graph::from_yaml(
        r#"
nodes:
  - name: "n"
    kernel: "PortProbe"
    input_ports: ["x", "y"]
    output_ports: ["out"]
    input_policy: { type: "sync_set", sets: [["x", "nope"]] }
input_ports: ["x", "y"]
output_ports: ["out"]
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("nonexistent input port"), "{err}");
}
