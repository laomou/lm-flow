//! Rust 算子编写糖:`trait Kernel` + `register_kernel::<T>()` + 安全 `KernelCtx` / `KernelContract`。
//!
//! 这是 C ABI 算子 vtable **之上的糖**(和 `include/flow.hpp` 的 `KernelAdapter<T>` 对 C++ 做的
//! 事一样),**不是**引擎绕过 vtable 的原生快路 —— 让 Rust 也能一等公民地写算子。
//!
//! 用法:实现 [`Kernel`],在 [`Graph::from_yaml`](crate::Graph::from_yaml) **之前**调
//! [`register_kernel`],之后 YAML 用 `kernel: <名字>` 引用。
//!
//! ```ignore
//! #[derive(Default)]
//! struct Double { factor: i64 }
//! impl lmflow::Kernel for Double {
//!     fn get_contract(c: &mut lmflow::KernelContract) {
//!         c.input_type(0, lmflow::packet::type_id::I64);
//!         c.output_type(0, lmflow::packet::type_id::I64);
//!     }
//!     fn open(&mut self, cc: &mut lmflow::KernelCtx) -> lmflow::Result<()> {
//!         self.factor = cc.option_i64("factor", 2);
//!         Ok(())
//!     }
//!     fn process(&mut self, cc: &mut lmflow::KernelCtx) -> lmflow::Result<()> {
//!         let v = cc.input(0).and_then(|p| p.as_i64()).ok_or_else(|| cc.fail("need i64"))?;
//!         cc.emit(0, lmflow::Packet::from_i64(v * self.factor))
//!     }
//! }
//! lmflow::register_kernel::<Double>("Double").unwrap();
//! ```

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::context::Context;
use crate::kernel::{register, Contract, KernelVTable};
use crate::packet::Packet;
use crate::status::{code, Error, Result};
use crate::timestamp::Timestamp;

/// 用 Rust 写算子的接口。`Default` 用于构造实例(引擎在 create 时 `T::default()`;真正的初始化
/// 读 options 放 [`open`](Kernel::open))。只有 [`process`](Kernel::process) 必需。
pub trait Kernel: Default + 'static {
    /// 声明端口类型 / 必需 side packet(可选;默认不声明 = 接受任意类型)。
    fn get_contract(_c: &mut KernelContract) {}
    /// 开图时调一次:读 options、分配长期状态。
    fn open(&mut self, _cc: &mut KernelCtx) -> Result<()> {
        Ok(())
    }
    /// 每次触发调用:读输入、产出。**必需**。
    fn process(&mut self, cc: &mut KernelCtx) -> Result<()>;
    /// 关流时调一次:收尾。
    fn close(&mut self, _cc: &mut KernelCtx) -> Result<()> {
        Ok(())
    }
}

/// 算子回调期的上下文(安全包装内部 `Context`;只在回调期有效)。
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
    /// 借用第 `idx` 个输入包(单包策略);`batch` 策略配合 [`input_count`](Self::input_count) /
    /// [`input_at`](Self::input_at) 遍历一批。
    pub fn input(&self, idx: usize) -> Option<&Packet> {
        self.inner.input(idx)
    }
    pub fn input_count(&self, idx: usize) -> usize {
        self.inner.input_count(idx)
    }
    pub fn input_at(&self, idx: usize, k: usize) -> Option<&Packet> {
        self.inner.input_at(idx, k)
    }
    /// 取走输入包(所有权移交,CoW 省拷贝的第一步)。
    pub fn take_input(&mut self, idx: usize) -> Packet {
        self.inner.take_input(idx)
    }
    /// 本次触发的(对齐)时间戳。
    pub fn input_timestamp(&self) -> Timestamp {
        self.inner.input_ts
    }
    /// 该输入口是否已终结(上游关闭且排空)。
    pub fn input_is_done(&self, idx: usize) -> bool {
        self.inner.inputs_done.get(idx).copied().unwrap_or(false)
    }
    /// 产出到第 `out_idx` 个输出口。
    pub fn emit(&mut self, out_idx: usize, pkt: Packet) -> Result<()> {
        self.inner.emit(out_idx, pkt)
    }
    /// 零拷贝直通:把第 `in_idx` 个输入转到第 `out_idx` 个输出。
    pub fn forward(&mut self, in_idx: usize, out_idx: usize) -> Result<()> {
        self.inner.forward(in_idx, out_idx)
    }
    /// 不产出时推进下游时间戳边界(否则下游会一直等)。
    pub fn set_next_bound(&mut self, out_idx: usize, bound: Timestamp) {
        self.inner.set_next_bound(out_idx, bound)
    }
    /// 源算子(0 输入)自报「已产完」→ 引擎关流终止。
    pub fn source_done(&mut self) {
        self.inner.source_done = true;
    }
    /// 设置失败原因(通常配合 `return Err(cc.fail(...))`)。
    pub fn set_error(&mut self, msg: &str) {
        self.inner.set_error(msg);
    }
    /// 构造一个带原因的算子错误,便于 `return Err(cc.fail("..."))`。
    pub fn fail(&self, msg: &str) -> Error {
        Error::Kernel(msg.to_string())
    }

    // ---- 日志与计数器(与 C++ 侧 `cc.Log` / `cc.CounterAdd` 对应)----
    /// 写一条引擎日志(级别见 [`crate::runtime`] 的 `LOG_*`)。走引擎的日志回调,
    /// 不抢占宿主 stdout。
    pub fn log(&self, level: i32, msg: &str) {
        self.inner.log(level, msg);
    }
    /// 是否装了日志 sink。**格式化之前先问一句** —— `format!` 会堆分配,没人听的时候
    /// 一分钱都不该花(算子里的日志常常是每包一条):
    ///
    /// ```ignore
    /// if cc.log_enabled() {
    ///     cc.log(LOG_DEBUG, &format!("frame {}", n));
    /// }
    /// ```
    pub fn log_enabled(&self) -> bool {
        crate::runtime::log_enabled()
    }
    /// 累加一个**按图**的命名计数器。比日志更适合被测试断言 ——
    /// 跑完用 [`crate::Graph::counter_value`] 读回。
    pub fn counter_add(&self, name: &str, delta: i64) {
        self.inner.shared.counter_add(name, delta);
    }

    // ---- node options(路径支持点号嵌套,如 "roi.x")----
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

/// 契约声明(安全包装内部 `Contract`;只在 `get_contract` 期有效)。
/// 类型 id 见 [`crate::packet::type_id`](crate::packet)(`I64` / `F64` / `BUFFER` … ;`0` = 任意)。
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
    /// 声明第 `i` 个输入口的类型(`0` = 任意;内建用 `crate::packet::type_id::I64` 等)。
    pub fn input_type(&mut self, i: usize, type_id: u64) {
        if let Some(t) = self.inner.input_types.get_mut(i) {
            *t = type_id;
        }
    }
    pub fn output_type(&mut self, i: usize, type_id: u64) {
        if let Some(t) = self.inner.output_types.get_mut(i) {
            *t = type_id;
        }
    }
    pub fn input_any(&mut self, i: usize) {
        self.input_type(i, 0);
    }
    pub fn output_any(&mut self, i: usize) {
        self.output_type(i, 0);
    }
    /// 声明必需的 side packet;宿主漏注入则 `init` 阶段报错。
    pub fn require_side_packet(&mut self, name: &str) {
        self.inner.required_side_packets.push(name.to_string());
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
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut kc = KernelContract {
            inner: &mut *(c as *mut Contract),
        };
        T::get_contract(&mut kc);
    }));
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

/// 注册一个 Rust 算子。**在 `Graph::from_yaml` 之前**调用一次;之后 YAML 用 `kernel: name` 引用。
/// 重名报错(与内置一致)。
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
