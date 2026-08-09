//! 运行期共享设施:日志接收器、图级共享状态、C 字符串驻留。

use std::collections::BTreeMap;
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::buffer_pool::BufferPool;
use crate::config::GraphConfig;
use crate::status::Error;

// ---------------------------------------------------------------- 日志

pub const LOG_ERROR: i32 = 0;
pub const LOG_WARN: i32 = 1;
pub const LOG_INFO: i32 = 2;
pub const LOG_DEBUG: i32 = 3;

type LogFn = unsafe extern "C" fn(*mut c_void, i32, *const std::ffi::c_char);

struct LogSink {
    cb: Option<LogFn>,
    user: *mut c_void,
}
// 安全性:user 是宿主提供的不透明指针,引擎只原样回传。
unsafe impl Send for LogSink {}

static LOG: Mutex<LogSink> = Mutex::new(LogSink {
    cb: None,
    user: std::ptr::null_mut(),
});

/// 是否装了 sink 的**无锁快路标志**。没装时(默认)`log()` 靠它直接返回,不必去抢
/// 那把进程级 `LOG` 互斥锁 —— 算子里的 `cc.Log` 可能是**每包**调用的,让默认零配置
/// 的用户每包抢一次全局锁是不能接受的。仅作快路提示,权威值仍在 `LOG` 里。
static LOG_SET: AtomicBool = AtomicBool::new(false);

pub fn set_log_callback(cb: Option<LogFn>, user: *mut c_void) {
    let mut sink = LOG.lock().expect("log lock poisoned");
    sink.cb = cb;
    sink.user = user;
    // 先改 sink 再放标志:置位后 log() 才会去读,故不会读到半成品。
    LOG_SET.store(cb.is_some(), Ordering::Release);
}

/// 当前是否装了日志 sink。**给算子做「格式化之前先问一句」用** ——
/// `format!` 会堆分配,没人听的时候一分钱都不该花:
///
/// ```ignore
/// if lmflow::runtime::log_enabled() {
///     cc.log(LOG_DEBUG, &format!("frame {}", n));   // 只在真有 sink 时才格式化
/// }
/// ```
pub fn log_enabled() -> bool {
    LOG_SET.load(Ordering::Acquire)
}

/// 打一条日志。**调用时不得持有任何引擎内部锁**(见 flow.h 日志一节的承诺):
/// 回调可能去抢 GIL 或加宿主自己的锁,持锁调用会形成锁序环。
pub fn log(level: i32, msg: &str) {
    // 快路:没装 sink 就别碰那把全局锁(见 LOG_SET)。
    if !LOG_SET.load(Ordering::Acquire) {
        return;
    }
    let (cb, user) = {
        let sink = LOG.lock().expect("log lock poisoned");
        (sink.cb, sink.user)
    };
    if let Some(f) = cb {
        if let Ok(c) = CString::new(msg) {
            unsafe { f(user, level, c.as_ptr()) };
        }
    }
}

pub fn log_warn(msg: &str) {
    log(LOG_WARN, msg);
}
pub fn log_info(msg: &str) {
    log(LOG_INFO, msg);
}

// ---------------------------------------------------------------- C 字符串驻留
//
// C ABI 里有不少返回 `const char*` 且「生命周期随 graph」的接口。
// 驻留后指针永久有效:CString 的堆缓冲不随容器搬移而失效。

#[derive(Default)]
pub struct CStrArena {
    map: Mutex<BTreeMap<String, CString>>,
}

impl CStrArena {
    /// 返回该字符串对应的、生命周期与本 arena 相同的 C 指针。
    pub fn intern(&self, s: &str) -> *const std::ffi::c_char {
        let mut m = self.map.lock().expect("string pool lock poisoned");
        let c = m
            .entry(s.to_string())
            .or_insert_with(|| CString::new(s).unwrap_or_default());
        c.as_ptr()
    }
}

/// 线程局部的「最近一次错误」文本,供 `lmflow_last_error` 返回。
pub mod last_error {
    use std::cell::RefCell;
    use std::ffi::{c_char, CString};

    thread_local! {
        static LAST: RefCell<CString> = RefCell::new(CString::default());
    }

    pub fn set(msg: &str) {
        let c = CString::new(msg).unwrap_or_default();
        LAST.with(|l| *l.borrow_mut() = c);
    }

    pub fn get() -> *const c_char {
        LAST.with(|l| l.borrow().as_ptr())
    }

    pub fn take() -> String {
        LAST.with(|l| l.borrow().to_string_lossy().into_owned())
    }
}

// ---------------------------------------------------------------- 图级共享状态

/// 关闭原因,与 `LMFlowCloseReason` 一致。
pub const CLOSE_NORMAL: i32 = 0;
pub const CLOSE_ERROR: i32 = 1;
pub const CLOSE_CANCELLED: i32 = 2;

pub struct GraphShared {
    pub config: GraphConfig,
    pub(crate) buffer_pool: std::sync::Arc<BufferPool>,
    /// 首个错误(后续错误只记日志,不覆盖首因)
    error: Mutex<Option<Error>>,
    error_c: CStrArena,
    has_error: AtomicBool,
    cancelled: AtomicBool,
    /// 全局水位统计
    total_queued: AtomicUsize,
    total_queued_bytes: AtomicU64,
    /// 算子自报计数器
    counters: Mutex<BTreeMap<String, i64>>,
    pub strings: CStrArena,
}

impl GraphShared {
    pub fn new(config: GraphConfig) -> Self {
        Self {
            buffer_pool: std::sync::Arc::new(BufferPool::new(config.buffer_pool_max_bytes)),
            config,
            error: Mutex::new(None),
            error_c: CStrArena::default(),
            has_error: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            total_queued: AtomicUsize::new(0),
            total_queued_bytes: AtomicU64::new(0),
            counters: Mutex::new(BTreeMap::new()),
            strings: CStrArena::default(),
        }
    }

    // ---- 错误 ----

    pub fn record_error(&self, e: Error) {
        let msg = e.to_string();
        let mut slot = self.error.lock().expect("error lock poisoned");
        if slot.is_none() {
            *slot = Some(e);
            self.has_error.store(true, Ordering::SeqCst);
            drop(slot); // 先解锁再打日志:日志回调不得在持锁时调用
            log(LOG_ERROR, &msg);
        } else {
            drop(slot);
            log(LOG_WARN, &format!("(subsequent error, ignored) {msg}"));
        }
    }

    /// 复位所有**运行期**状态,供图 reset 重跑用(`config` 等构建期数据不动)。
    /// `record_error` 只「首因生效、不覆盖」,没有反向清除路径 —— 这里补上,
    /// 否则 reset 后的图会带着上一轮的 error / cancelled 出生(start 立刻被挡回)。
    ///
    /// 只应在图**静止**时调用(Terminated + is_idle,见 `GraphInner::reset`),故用
    /// `&self` + 内部可变即可,无并发。
    pub fn reset_run_state(&self) {
        *self.error.lock().expect("error lock poisoned") = None;
        self.has_error.store(false, Ordering::SeqCst);
        self.cancelled.store(false, Ordering::SeqCst);
        self.total_queued.store(0, Ordering::Relaxed);
        self.total_queued_bytes.store(0, Ordering::Relaxed);
        // 算子自报计数器:新一轮从零计(诊断语义;要跨轮累计的宿主自己在算子里存)。
        self.counters.lock().expect("counter lock poisoned").clear();
    }

    pub fn has_error(&self) -> bool {
        self.has_error.load(Ordering::SeqCst)
    }

    pub fn first_error(&self) -> Option<Error> {
        self.error.lock().expect("error lock poisoned").clone()
    }

    /// 图级错误文本的 C 指针 —— 注意 `lmflow_last_error` 是线程局部的,
    /// 工作线程上算子的失败原因只能通过这里拿到。
    pub fn error_cstr(&self) -> *const std::ffi::c_char {
        let msg = self
            .first_error()
            .map(|e| e.to_string())
            .unwrap_or_default();
        self.error_c.intern(&msg)
    }

    // ---- 取消 ----

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 算子 close 时应告知的原因,供其决定是否提交结果。
    pub fn close_reason(&self) -> i32 {
        if self.is_cancelled() {
            CLOSE_CANCELLED
        } else if self.has_error() {
            CLOSE_ERROR
        } else {
            CLOSE_NORMAL
        }
    }

    // ---- 全局水位 ----
    //
    // 这几个是**纯计数器**:除了自己被读回来跟阈值比一下(`over_watermark`),不承载任何
    // 「发布数据」的语义 —— 没有任何东西经由它们建立 happens-before。所以用 `Relaxed`。
    //
    // 这不是抠常数:`on_enqueue`/`on_dequeue` 是**每包每消费者**都要走的,一包就是 4 次
    // 原子 RMW。x86 上 `lock xadd` 与 ordering 无关,但 **aarch64 上 SeqCst 的 RMW 会生成
    // 带全屏障的 `ldaxr/stlxr`**,而 Android / iOS arm64 正是发布目标。
    //
    // ⚠ 全库其余的 SeqCst 都**审过并有意保留**:终止判定(`input_closed[i] && queue_len==0`
    // 这种两段式检查)、`has_error` / `cancelled` / `source_done` 的发布、执行器 stop 标志、
    // 边与 poller 的 closed —— 那些是真同步,降级会坏掉终止正确性。别顺手一起改。

    pub fn on_enqueue(&self, bytes: u64) {
        self.total_queued.fetch_add(1, Ordering::Relaxed);
        self.total_queued_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn on_dequeue(&self, bytes: u64) {
        // 用 saturating 语义,避免任何计数不平衡导致下溢回绕成天文数字
        let _ = self
            .total_queued
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
        let _ = self
            .total_queued_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(bytes))
            });
    }

    pub fn total_queued(&self) -> usize {
        self.total_queued.load(Ordering::Relaxed)
    }
    pub fn total_queued_bytes(&self) -> u64 {
        self.total_queued_bytes.load(Ordering::Relaxed)
    }

    /// 是否已触及全局水位。超限时把压力转化为**图输入口**背压(只在入口刹车不会
    /// 引入 diamond 死锁,见 docs/design.md §7.5)。
    ///
    /// 水位是**软阈值**:读的是 `Relaxed` 快照,故判定可能滞后一两个包 —— 这正是
    /// 「水位」该有的语义,不需要精确的那一瞬间。
    pub fn over_watermark(&self) -> bool {
        let c = &self.config;
        c.max_queued_packets > 0 && self.total_queued() >= c.max_queued_packets
    }

    // ---- 计数器 ----

    pub fn counter_add(&self, name: &str, delta: i64) {
        let mut m = self.counters.lock().expect("counter lock poisoned");
        *m.entry(name.to_string()).or_insert(0) += delta;
    }
    pub fn counter_value(&self, name: &str) -> i64 {
        self.counters
            .lock()
            .expect("counter lock poisoned")
            .get(name)
            .copied()
            .unwrap_or(0)
    }
    pub fn counter_names(&self) -> Vec<String> {
        self.counters
            .lock()
            .expect("counter lock poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GraphConfig;

    fn shared() -> GraphShared {
        GraphShared::new(GraphConfig::from_yaml("nodes: []").unwrap())
    }

    #[test]
    fn first_error_wins() {
        let s = shared();
        assert!(!s.has_error());
        s.record_error(Error::Kernel("first".into()));
        s.record_error(Error::Kernel("second".into()));
        assert!(s.has_error());
        // 首因不被覆盖 —— 后续错误往往是首因的连锁反应
        assert!(s.first_error().unwrap().to_string().contains("first"));
    }

    #[test]
    fn close_reason_priority() {
        let s = shared();
        assert_eq!(s.close_reason(), CLOSE_NORMAL);
        s.record_error(Error::Kernel("x".into()));
        assert_eq!(s.close_reason(), CLOSE_ERROR);
        s.cancel();
        assert_eq!(
            s.close_reason(),
            CLOSE_CANCELLED,
            "cancelled takes priority over error"
        );
    }

    #[test]
    fn watermark_counts_packets_and_bytes() {
        let cfg = GraphConfig::from_yaml("nodes: []\nmax_queued_packets: 2").unwrap();
        let s = GraphShared::new(cfg);
        assert!(!s.over_watermark());
        s.on_enqueue(10);
        assert!(!s.over_watermark());
        s.on_enqueue(10);
        assert!(
            s.over_watermark(),
            "reaching the limit counts as over watermark"
        );
        s.on_dequeue(10);
        assert!(!s.over_watermark());
        assert_eq!(s.total_queued(), 1);
        assert_eq!(s.total_queued_bytes(), 10);
    }

    #[test]
    fn dequeue_never_underflows() {
        let s = shared();
        // 即使计数不平衡也不能回绕成天文数字(会让水位判断永久为真)
        s.on_dequeue(999);
        assert_eq!(s.total_queued(), 0);
        assert_eq!(s.total_queued_bytes(), 0);
    }

    #[test]
    fn counters_accumulate() {
        let s = shared();
        s.counter_add("frames", 1);
        s.counter_add("frames", 2);
        assert_eq!(s.counter_value("frames"), 3);
        assert_eq!(s.counter_value("nope"), 0);
        assert_eq!(s.counter_names(), vec!["frames".to_string()]);
    }

    #[test]
    fn interned_pointers_stay_valid_after_more_inserts() {
        let a = CStrArena::default();
        let p1 = a.intern("first");
        for i in 0..100 {
            a.intern(&format!("filler{i}"));
        }
        let s = unsafe { std::ffi::CStr::from_ptr(p1) };
        assert_eq!(
            s.to_str().unwrap(),
            "first",
            "interned pointer must stay valid after subsequent inserts"
        );
    }
}
