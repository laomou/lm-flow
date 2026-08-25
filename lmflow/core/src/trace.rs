//! 逐次调用的执行 trace:有界事件环 + Chrome Trace 导出。
//!
//! 默认关闭(`config.trace_capacity == 0`)。开启时,每次算子回调(Open/Process/Close)记
//! 一条 span 进有界环 —— 环满丢最旧,故内存有界。可经 [`to_chrome_trace_json`] 导出成
//! chrome://tracing / perfetto 可读的 "Trace Event Format"(complete `"X"` 事件:含 `ts` +
//! `dur`),从而看到「哪个节点在什么时刻、在哪个线程上跑了多久」的时间线 —— 这是聚合统计
//! (总时长/最大/百分位)回答不了的。
//!
//! 并发:多个 worker 线程并发记录,整个环用一把 `Mutex` 保护。trace 是调试/剖析模式,这点
//! 争用可接受,换来的是显然正确、tsan 干净。**关闭时热路径零开销** —— `GraphShared::trace`
//! 为 `None` 时,scheduler 连时钟都不读(见 `call_kernel`)。

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::thread::ThreadId;

/// 算子回调阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TracePhase {
    Open,
    Process,
    Close,
}

impl TracePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            TracePhase::Open => "open",
            TracePhase::Process => "process",
            TracePhase::Close => "close",
        }
    }
}

/// 一条执行 span。时间均相对图 epoch,单位微秒。
#[derive(Clone, Copy, Debug)]
pub struct TraceEvent {
    /// 节点索引(导出时解析成节点名/算子名)。
    pub node: u32,
    pub phase: TracePhase,
    /// 该图内稳定分配的小整数线程号(chrome 的泳道 tid)。
    pub tid: u32,
    /// 相对图 epoch 的起始微秒。
    pub start_us: i64,
    /// 本次回调时长(微秒)。
    pub dur_us: i64,
    /// 本次激活的对齐时间戳原值;Open/Close 阶段可能是 UNSET 哨兵。
    pub input_ts: i64,
}

struct Inner {
    events: VecDeque<TraceEvent>,
    capacity: usize,
    tids: HashMap<ThreadId, u32>,
    next_tid: u32,
}

/// 有界 trace 事件环(FIFO,满了丢最旧)。
pub struct TraceRing {
    inner: Mutex<Inner>,
}

impl TraceRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                events: VecDeque::with_capacity(capacity.min(1024)),
                capacity: capacity.max(1),
                tids: HashMap::new(),
                next_tid: 0,
            }),
        }
    }

    /// 记一条 span。满了丢最旧。线程号在本环内稳定分配(首次见到某线程即给一个递增小整数)。
    pub fn record(&self, node: u32, phase: TracePhase, start_us: i64, dur_us: i64, input_ts: i64) {
        let id = std::thread::current().id();
        let mut g = self.inner.lock().expect("trace ring poisoned");
        let tid = match g.tids.get(&id) {
            Some(&t) => t,
            None => {
                let t = g.next_tid;
                g.next_tid += 1;
                g.tids.insert(id, t);
                t
            }
        };
        if g.events.len() >= g.capacity {
            g.events.pop_front();
        }
        g.events.push_back(TraceEvent {
            node,
            phase,
            tid,
            start_us,
            dur_us,
            input_ts,
        });
    }

    /// 当前环内容的快照(**不清空**:环自身有界,可多次导出)。
    pub fn snapshot(&self) -> Vec<TraceEvent> {
        let g = self.inner.lock().expect("trace ring poisoned");
        g.events.iter().copied().collect()
    }

    /// 清空(供图 reset 重跑;事件、线程号映射一并归零,容量不变)。
    pub fn clear(&self) {
        let mut g = self.inner.lock().expect("trace ring poisoned");
        g.events.clear();
        g.tids.clear();
        g.next_tid = 0;
    }
}

/// 把 JSON 字符串值需要转义的字符转义(引号/反斜杠/控制字符)。节点名/算子名由宿主定,
/// 可能含任意字符,故必须转义,否则产出的不是合法 JSON。
fn push_json_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 把 span 导出成 Chrome Trace Event Format(chrome://tracing / perfetto 可直接打开)。
///
/// `labels[node]` = `(节点名, 算子名)`。产出 complete `"X"` 事件,时间单位微秒,`pid = 1`,
/// `tid` 为环内线程号。手写 JSON(字段少)以免给 core 引入 `serde_json` 依赖。
pub fn to_chrome_trace_json(events: &[TraceEvent], labels: &[(String, String)]) -> String {
    let mut out = String::with_capacity(64 + events.len() * 128);
    out.push_str("{\"traceEvents\":[");
    for (i, e) in events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (node_name, kernel_name) = labels
            .get(e.node as usize)
            .map(|(n, k)| (n.as_str(), k.as_str()))
            .unwrap_or(("<unknown>", ""));
        out.push('{');
        out.push_str("\"name\":");
        push_json_escaped(&mut out, node_name);
        out.push_str(",\"cat\":");
        push_json_escaped(&mut out, e.phase.as_str());
        out.push_str(",\"ph\":\"X\",\"pid\":1,\"tid\":");
        let _ = write!(out, "{}", e.tid);
        out.push_str(",\"ts\":");
        let _ = write!(out, "{}", e.start_us);
        out.push_str(",\"dur\":");
        let _ = write!(out, "{}", e.dur_us.max(0));
        out.push_str(",\"args\":{\"kernel\":");
        push_json_escaped(&mut out, kernel_name);
        out.push_str(",\"phase\":");
        push_json_escaped(&mut out, e.phase.as_str());
        out.push_str(",\"input_ts\":");
        let _ = write!(out, "{}", e.input_ts);
        out.push_str("}}");
    }
    out.push_str("],\"displayTimeUnit\":\"ms\"}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_and_drops_oldest() {
        let ring = TraceRing::new(2);
        ring.record(0, TracePhase::Process, 0, 5, 100);
        ring.record(1, TracePhase::Process, 10, 5, 200);
        ring.record(2, TracePhase::Process, 20, 5, 300);
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 2, "capacity 2 keeps only 2");
        assert_eq!(snap[0].node, 1, "oldest (node 0) was evicted");
        assert_eq!(snap[1].node, 2);
    }

    #[test]
    fn clear_resets_events_and_tids() {
        let ring = TraceRing::new(4);
        ring.record(0, TracePhase::Open, 0, 1, 0);
        ring.clear();
        assert!(ring.snapshot().is_empty());
        // 清空后线程号重新从 0 开始
        ring.record(0, TracePhase::Process, 0, 1, 0);
        assert_eq!(ring.snapshot()[0].tid, 0);
    }

    #[test]
    fn chrome_json_escapes_and_wraps() {
        let events = vec![TraceEvent {
            node: 0,
            phase: TracePhase::Process,
            tid: 3,
            start_us: 12,
            dur_us: 34,
            input_ts: 1000,
        }];
        let labels = vec![("no\"de".to_string(), "Kern\\el".to_string())];
        let json = to_chrome_trace_json(&events, &labels);
        assert!(json.starts_with("{\"traceEvents\":["));
        assert!(json.contains("\"ts\":12"));
        assert!(json.contains("\"dur\":34"));
        assert!(json.contains("\"tid\":3"));
        assert!(json.contains("no\\\"de"), "name quote escaped");
        assert!(json.contains("Kern\\\\el"), "kernel backslash escaped");
        assert!(json.contains("\"input_ts\":1000"));
    }
}
