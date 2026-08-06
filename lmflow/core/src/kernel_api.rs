//! Sugar for writing kernels in Rust: [`Kernel`] + [`register_kernel`], plus the safe
//! [`KernelCtx`] / [`KernelContract`] wrappers.
//!
//! This layer sits **on top of** the C ABI kernel vtable — the same thing
//! `include/lmflow/flow.hpp`'s `KernelAdapter<T>` does for C++. It is not a native fast path that
//! bypasses the vtable; its purpose is to make Rust a first-class language for writing kernels.
//!
//! Implement [`Kernel`], call [`register_kernel`] **before**
//! [`Graph::from_yaml`](crate::Graph::from_yaml), then refer to the kernel by name from YAML as
//! `kernel: <name>`.
//!
//! ```
//! use lmflow::packet::type_id;
//! use lmflow::{register_kernel, Kernel, KernelContract, KernelCtx, Packet};
//!
//! #[derive(Default)]
//! struct Scale {
//!     factor: i64,
//! }
//!
//! impl Kernel for Scale {
//!     fn get_contract(c: &mut KernelContract) {
//!         c.input_type(0, type_id::I64);
//!         c.output_type(0, type_id::I64);
//!     }
//!
//!     fn open(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
//!         self.factor = cc.option_i64("factor", 2);
//!         Ok(())
//!     }
//!
//!     fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
//!         let v = cc
//!             .input(0)
//!             .and_then(|p| p.as_i64())
//!             .ok_or_else(|| cc.fail("input 0 must be an i64"))?;
//!         cc.emit(0, Packet::from_i64(v * self.factor))
//!     }
//! }
//!
//! # fn main() -> lmflow::Result<()> {
//! register_kernel::<Scale>("Scale")?;
//! # Ok(())
//! # }
//! ```

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::context::Context;
use crate::kernel::{register, Contract, KernelVTable};
use crate::packet::Packet;
use crate::status::{code, Error, Result};
use crate::timestamp::Timestamp;

/// The interface for writing a kernel in Rust.
///
/// `Default` is how the engine constructs an instance (`T::default()` at create time); real
/// initialisation — reading node options, allocating long-lived state — belongs in
/// [`open`](Kernel::open). Only [`process`](Kernel::process) is required.
pub trait Kernel: Default + 'static {
    /// Declare port types and required side packets. Optional: declaring nothing means every
    /// port accepts any payload type.
    fn get_contract(_c: &mut KernelContract) {}
    /// Called once when the graph starts: read options, allocate long-lived state.
    fn open(&mut self, _cc: &mut KernelCtx) -> Result<()> {
        Ok(())
    }
    /// Called on every activation: read the inputs, produce output. **Required.**
    fn process(&mut self, cc: &mut KernelCtx) -> Result<()>;
    /// Called once when the stream closes: flush, release, finalise.
    fn close(&mut self, _cc: &mut KernelCtx) -> Result<()> {
        Ok(())
    }
}

/// The context of one kernel callback — a safe wrapper around the internal `Context`.
///
/// Valid **only for the duration of the callback**; it must not be stored. While a kernel runs the
/// engine holds no internal lock, so a kernel is free to block, lock, or acquire the GIL here.
pub struct KernelCtx<'a> {
    inner: &'a mut Context,
}

impl KernelCtx<'_> {
    pub fn num_inputs(&self) -> usize {
        self.inner.in_ports.len()
    }
    pub fn num_outputs(&self) -> usize {
        self.inner.out_ports.len()
    }
    /// Borrow input packet `idx` (single-packet policies). Under the `batch` policy use
    /// [`input_count`](Self::input_count) / [`input_at`](Self::input_at) to walk the batch.
    pub fn input(&self, idx: usize) -> Option<&Packet> {
        self.inner.input(idx)
    }
    pub fn input_count(&self, idx: usize) -> usize {
        self.inner.input_count(idx)
    }
    pub fn input_at(&self, idx: usize, k: usize) -> Option<&Packet> {
        self.inner.input_at(idx, k)
    }
    /// Take ownership of an input packet — the first step to a copy-free mutation, since
    /// copy-on-write only avoids the copy when this is the sole reference.
    pub fn take_input(&mut self, idx: usize) -> Packet {
        self.inner.take_input(idx)
    }
    /// The (aligned) timestamp of this activation.
    pub fn input_timestamp(&self) -> Timestamp {
        self.inner.input_ts
    }
    /// Whether this input port is finished — upstream closed and drained.
    pub fn input_is_done(&self, idx: usize) -> bool {
        self.inner.inputs_done.get(idx).copied().unwrap_or(false)
    }
    /// Produce a packet on output port `out_idx`.
    pub fn emit(&mut self, out_idx: usize, pkt: Packet) -> Result<()> {
        self.inner.emit(out_idx, pkt)
    }
    /// Zero-copy passthrough: forward input `in_idx` to output `out_idx`.
    pub fn forward(&mut self, in_idx: usize, out_idx: usize) -> Result<()> {
        self.inner.forward(in_idx, out_idx)
    }
    /// Advance the downstream timestamp bound when producing nothing — otherwise consumers keep
    /// waiting for a packet that will never arrive.
    pub fn set_next_bound(&mut self, out_idx: usize, bound: Timestamp) {
        self.inner.set_next_bound(out_idx, bound)
    }
    /// For a source kernel (no inputs): report that it has produced everything, so the engine
    /// closes the stream and terminates.
    pub fn source_done(&mut self) {
        self.inner.source_done = true;
    }
    /// For a source kernel (no inputs): cooperatively release the worker and ask the engine to
    /// invoke the source again after `delay`.
    pub fn source_yield(&mut self, delay: std::time::Duration) {
        self.inner.source_yield = Some(delay);
    }
    /// Record a failure reason (usually paired with `return Err(cc.fail(...))`).
    pub fn set_error(&mut self, msg: &str) {
        self.inner.set_error(msg);
    }
    /// Build a kernel error carrying a reason, for `return Err(cc.fail("..."))`.
    pub fn fail(&self, msg: &str) -> Error {
        Error::Kernel(msg.to_string())
    }

    // ---- 日志与计数器(与 C++ 侧 `cc.Log` / `cc.CounterAdd` 对应)----
    /// Write an engine log line (levels are the `LOG_*` constants in [`crate::runtime`]). This
    /// goes through the engine's log callback rather than stealing the host's stdout.
    pub fn log(&self, level: i32, msg: &str) {
        self.inner.log(level, msg);
    }
    /// Whether a log sink is installed. **Ask before formatting** — `format!` allocates, and
    /// kernel logging is often per-packet, so it should cost nothing when nobody is listening:
    ///
    /// ```ignore
    /// if cc.log_enabled() {
    ///     cc.log(LOG_DEBUG, &format!("frame {}", n));
    /// }
    /// ```
    pub fn log_enabled(&self) -> bool {
        crate::runtime::log_enabled()
    }
    /// Add to a named **per-graph** counter. Easier to assert on than a log line — read it back
    /// afterwards with [`crate::Graph::counter_value`].
    pub fn counter_add(&self, name: &str, delta: i64) {
        self.inner.shared.counter_add(name, delta);
    }

    // ---- node options(路径支持点号嵌套,如 "roi.x")----
    /// Whether the node declared this option. Paths may be dotted, e.g. `"roi.x"`.
    pub fn has_option(&self, key: &str) -> bool {
        self.inner.options.has(key)
    }
    pub fn option_i64(&self, key: &str, def: i64) -> i64 {
        self.inner.options.i64(key).unwrap_or(def)
    }
    pub fn option_f64(&self, key: &str, def: f64) -> f64 {
        self.inner.options.f64(key).unwrap_or(def)
    }
    pub fn option_bool(&self, key: &str, def: bool) -> bool {
        self.inner.options.bool(key).unwrap_or(def)
    }
    pub fn option_str<'s>(&'s self, key: &str, def: &'s str) -> &'s str {
        self.inner.options.str(key).unwrap_or(def)
    }
}

/// The port-contract declaration handed to [`Kernel::get_contract`] — a safe wrapper around the
/// internal `Contract`, valid only for the duration of that call.
///
/// Type ids live in [`crate::packet::type_id`](crate::packet) (`I64` / `F64` / `BUFFER` / …);
/// `0` means "any type". Invalid indexes, invalid type ids, duplicate/empty side-packet names,
/// and panics make graph construction fail instead of being ignored.
pub struct KernelContract<'a> {
    inner: &'a mut Contract,
}

impl KernelContract<'_> {
    pub fn num_inputs(&self) -> usize {
        self.inner.input_types.len()
    }
    pub fn num_outputs(&self) -> usize {
        self.inner.output_types.len()
    }
    /// Declare the payload type of input port `i` (`0` = any; use
    /// `crate::packet::type_id::I64` and friends for the built-in types).
    pub fn input_type(&mut self, i: usize, type_id: u64) {
        if let Some(t) = self.inner.input_types.get_mut(i) {
            *t = type_id;
        } else {
            self.inner.record_error(format!(
                "input port index {i} is out of range (num_inputs={})",
                self.inner.input_types.len()
            ));
        }
    }
    /// Declare the payload type of output port `i`. See [`input_type`](Self::input_type).
    pub fn output_type(&mut self, i: usize, type_id: u64) {
        if let Some(t) = self.inner.output_types.get_mut(i) {
            *t = type_id;
        } else {
            self.inner.record_error(format!(
                "output port index {i} is out of range (num_outputs={})",
                self.inner.output_types.len()
            ));
        }
    }
    /// Accept any payload type on input port `i` (the default).
    pub fn input_any(&mut self, i: usize) {
        self.input_type(i, 0);
    }
    /// Produce any payload type on output port `i` (the default).
    pub fn output_any(&mut self, i: usize) {
        self.output_type(i, 0);
    }
    /// Declare a required side packet; if the host forgets to inject it, graph init fails.
    pub fn require_side_packet(&mut self, name: &str) {
        if name.is_empty() {
            self.inner
                .record_error("required side packet name must not be empty");
        } else {
            self.inner.required_side_packets.push(name.to_string());
        }
    }
}

// ---------------------------------------------------------------- 泛型 extern "C" 蹦床
// 签名对齐 crate::kernel::KernelVTable(ctx/contract 用 *mut c_void)。异常/panic 绝不穿越
// extern "C" 边界:catch_unwind 兜住,失败返回状态码(与 flow.hpp 的 try/catch 对称)。

unsafe extern "C" fn tramp_create<T: Kernel>(_factory: *mut c_void) -> *mut c_void {
    match catch_unwind(|| Box::into_raw(Box::<T>::default()) as *mut c_void) {
        Ok(p) => p,
        Err(_) => std::ptr::null_mut(), // 构造 panic → null self;后续 open/process 因此报错、图失败
    }
}

unsafe extern "C" fn tramp_destroy<T: Kernel>(self_: *mut c_void) {
    if !self_.is_null() {
        drop(Box::from_raw(self_ as *mut T));
    }
}

unsafe extern "C" fn tramp_get_contract<T: Kernel>(_self: *mut c_void, c: *mut c_void) {
    if c.is_null() {
        return;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut kc = KernelContract {
            inner: &mut *(c as *mut Contract),
        };
        T::get_contract(&mut kc);
    }));
    if let Err(payload) = result {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Rust panic with non-string payload".to_string());
        (*(c as *mut Contract)).record_error(format!("Rust panic: {message}"));
    }
}

/// open / process / close 三个 phase 的公共骨架。
unsafe fn run_phase<T: Kernel>(
    self_: *mut c_void,
    ctx: *mut c_void,
    f: impl FnOnce(&mut T, &mut KernelCtx) -> Result<()>,
) -> i32 {
    if self_.is_null() || ctx.is_null() {
        return code::KERNEL; // create 失败(构造 panic)等
    }
    let r = catch_unwind(AssertUnwindSafe(|| {
        let k = &mut *(self_ as *mut T);
        let mut cc = KernelCtx {
            inner: &mut *(ctx as *mut Context),
        };
        match f(k, &mut cc) {
            Ok(()) => code::OK,
            Err(e) => {
                cc.inner.set_error(&e.to_string());
                e.code()
            }
        }
    }));
    match r {
        Ok(rc) => rc,
        Err(_) => {
            // 闭包已 unwind(其 &mut 借用随之释放),重新借 ctx 记录原因(该槽调用期独占存活)。
            (*(ctx as *mut Context)).set_error("kernel panicked");
            code::PANIC
        }
    }
}

unsafe extern "C" fn tramp_open<T: Kernel>(self_: *mut c_void, ctx: *mut c_void) -> i32 {
    run_phase::<T>(self_, ctx, |k, cc| k.open(cc))
}
unsafe extern "C" fn tramp_process<T: Kernel>(self_: *mut c_void, ctx: *mut c_void) -> i32 {
    run_phase::<T>(self_, ctx, |k, cc| k.process(cc))
}
unsafe extern "C" fn tramp_close<T: Kernel>(self_: *mut c_void, ctx: *mut c_void) -> i32 {
    run_phase::<T>(self_, ctx, |k, cc| k.close(cc))
}

/// Register a Rust kernel under `name`.
///
/// Call once **before [`Graph::from_yaml`](crate::Graph::from_yaml)**; YAML then refers to it as
/// `kernel: name`. Registering a name twice is an error, as it is for the built-ins.
pub fn register_kernel<T: Kernel>(name: &str) -> Result<()> {
    let vt = KernelVTable {
        create: Some(tramp_create::<T>),
        get_contract: Some(tramp_get_contract::<T>),
        open: Some(tramp_open::<T>),
        process: Some(tramp_process::<T>),
        close: Some(tramp_close::<T>),
        destroy: Some(tramp_destroy::<T>),
    };
    register(name, vt, std::ptr::null_mut())
}
