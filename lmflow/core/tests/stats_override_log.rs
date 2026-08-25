//! `stats` 被强制提升为 `full` 时,必须打一条 INFO 说明是**哪个**开关触发的 —— 否则宿主
//! 只看到自己配的 `stats` 悄悄失效,无从排查。
//!
//! 这条契约写在 `docs/web/cpp.md` 的 stats 一节里。断言 `effective_stats_level()` 抓不到它:
//! 提升本身是对的,漏的是**告知**。故这里真去装一个日志 sink、建图、看实际打出来的行。
//!
//! 日志 sink 是**进程级**全局状态,而同一测试二进制内的 `#[test]` 是并行跑的 —— 所以本文件
//! 只放一个测试,让它独占自己的进程。

mod common;

use std::ffi::{c_char, c_void, CStr};

/// 把引擎打出的每行日志收进 `user` 指向的 `Vec<String>`。
unsafe extern "C" fn capture(user: *mut c_void, _level: i32, msg: *const c_char) {
    if user.is_null() || msg.is_null() {
        return;
    }
    let sink = &mut *(user as *mut Vec<String>);
    sink.push(CStr::from_ptr(msg).to_string_lossy().into_owned());
}

/// 建一次图,返回期间打出的所有日志行。
fn build_and_capture_logs(prelude: &str) -> Vec<String> {
    let yaml = format!(
        "{prelude}
nodes:
  - {{ name: mid, kernel: PassThrough, input_ports: [in], output_ports: [out] }}
input_ports: [in]
output_ports: [out]
"
    );
    // 用 Box 而非局部变量的裸指针:避免「回调持 &mut 期间外层又读」的别名问题。
    let raw = Box::into_raw(Box::new(Vec::<String>::new()));
    lmflow::runtime::set_log_callback(Some(capture), raw as *mut c_void);
    let graph = common::graph_from_yaml(&yaml);
    lmflow::runtime::set_log_callback(None, std::ptr::null_mut());
    drop(graph.expect("graph should build"));
    // 安全性:sink 已摘除,回调不会再写;此处独占取回。
    unsafe { *Box::from_raw(raw) }
}

fn overridden_line(lines: &[String]) -> Option<&String> {
    lines.iter().find(|l| l.contains("overridden to full"))
}

#[test]
fn stats_override_names_the_switch_that_forced_full() {
    // trace 单独触发:这是回归的重点 —— 修前这里一条日志都不打(watchdog 分支不成立,
    // 而 `else if stats_level != Full` 因为已被提升为 Full 也不成立)。
    let lines = build_and_capture_logs("trace_capacity: 8");
    let line = overridden_line(&lines)
        .unwrap_or_else(|| panic!("trace_capacity > 0 应说明 stats 被提升, 实际日志: {lines:?}"));
    assert!(
        line.contains("trace_capacity > 0"),
        "应点名 trace_capacity: {line}"
    );
    assert!(
        !line.contains("watchdog_ms"),
        "没配 watchdog 就不该提它: {line}"
    );

    // watchdog 单独触发:原有行为,不能被这次改动弄坏。
    let lines = build_and_capture_logs("watchdog_ms: 1");
    let line = overridden_line(&lines)
        .unwrap_or_else(|| panic!("watchdog_ms > 0 应说明 stats 被提升, 实际日志: {lines:?}"));
    assert!(
        line.contains("watchdog_ms > 0"),
        "应点名 watchdog_ms: {line}"
    );
    assert!(
        !line.contains("trace_capacity"),
        "没配 trace 就不该提它: {line}"
    );

    // 两个都配:两个都要点名,否则关掉一个的人会以为 stats 就恢复了。
    let lines = build_and_capture_logs("watchdog_ms: 1\ntrace_capacity: 8");
    let line = overridden_line(&lines)
        .unwrap_or_else(|| panic!("两者同时开应说明 stats 被提升, 实际日志: {lines:?}"));
    assert!(
        line.contains("watchdog_ms > 0") && line.contains("trace_capacity > 0"),
        "两个开关都应点名: {line}"
    );

    // 用户自己就要 full:没有「覆盖」这回事,不该谎报。
    let lines = build_and_capture_logs("stats: full\ntrace_capacity: 8");
    assert!(
        overridden_line(&lines).is_none(),
        "显式配 full 时无覆盖可言: {lines:?}"
    );

    // 都没配:走另一条分支,提示 full 项被禁用(不是「被覆盖」)。
    let lines = build_and_capture_logs("");
    assert!(
        overridden_line(&lines).is_none(),
        "没有开关触发时不该报覆盖: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("stats is not full")),
        "默认 basic 应提示 full 项被禁用: {lines:?}"
    );
}
