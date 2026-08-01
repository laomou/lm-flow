//! C ABI 层:`include/flow.h` 的实现。
//!
//! 约定
//!  * 每个导出函数都用 `catch_unwind` 包裹 —— Rust panic 穿越 FFI 是 UB。
//!  * 失败时写线程局部 `lmflow_last_error`,让调用方能拿到可读原因。
//!  * 跨界结构体 `#[repr(C)]`,布局由 `tests/abi_layout.rs` 与 `cpp/abi_assert.cc` 双向钉死。
//!  * 所有导出函数对空指针都做检查,返回错误码/默认值而不是崩溃。
//!
//! 关于 `missing_safety_doc`:本模块的导出函数面向 **C 调用方**,其安全契约(指针有效性、
//! 所有权移交、生命周期)以 `include/flow.h` 的注释为权威定义 —— 那才是 C/C++ 用户会读的
//! 文档。在此重复一遍 Rust 风格的 `# Safety` 段落只会造成两处描述漂移,故整体豁免该 lint。
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use crate::context::Context;
use crate::graph::{Graph, GraphInner, Poller, State};
use crate::kernel::{Contract, KernelVTable};
use crate::packet::{self, BufferData, Builtin, Packet, Payload};
use crate::runtime::{self, last_error};
use crate::status::{code, Error};
use crate::timestamp::Timestamp;

pub const ABI_VERSION: u32 = 1;

pub const INVALID_ID: usize = usize::MAX;

// ---------------------------------------------------------------- 工具

fn guard<F: FnOnce() -> i32>(f: F) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            last_error::set("internal engine panic (caught at the FFI boundary)");
            code::PANIC
        }
    }
}

fn guard_val<T, F: FnOnce() -> T>(default: T, f: F) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            last_error::set("internal engine panic (caught at the FFI boundary)");
            default
        }
    }
}

fn fail(e: Error) -> i32 {
    last_error::set(&e.to_string());
    e.code()
}

fn to_status(r: crate::status::Result<()>) -> i32 {
    match r {
        Ok(()) => code::OK,
        Err(e) => fail(e),
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

unsafe fn ctx_ref<'a>(c: *const LMFlowContext) -> Option<&'a Context> {
    if c.is_null() {
        None
    } else {
        Some(&*(c as *const Context))
    }
}

unsafe fn ctx_mut<'a>(c: *mut LMFlowContext) -> Option<&'a mut Context> {
    if c.is_null() {
        None
    } else {
        Some(&mut *(c as *mut Context))
    }
}

unsafe fn contract_mut<'a>(c: *mut LMFlowContract) -> Option<&'a mut Contract> {
    if c.is_null() {
        None
    } else {
        Some(&mut *(c as *mut Contract))
    }
}

// ---------------------------------------------------------------- 不透明句柄

#[repr(C)]
pub struct LMFlowGraph {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowInput {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowPoller {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowContext {
    _private: [u8; 0],
}
#[repr(C)]
pub struct LMFlowContract {
    _private: [u8; 0],
}

/// 图输入口句柄的实体(生命周期随 graph)。
pub struct InputHandle {
    graph: Arc<GraphInner>,
    edge: usize,
}

// ---------------------------------------------------------------- Packet 跨界

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LMFlowPacket {
    pub payload: *mut c_void,
    pub type_id: u64,
    pub timestamp: i64,
    pub owner: *mut c_void,
    pub drop_fn: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl Default for LMFlowPacket {
    fn default() -> Self {
        Self {
            payload: std::ptr::null_mut(),
            type_id: 0,
            timestamp: Timestamp::unset().0,
            owner: std::ptr::null_mut(),
            drop_fn: None,
        }
    }
}

/// **借用**形态:不增加引用计数,调用方不得 drop。
pub fn borrow_packet(p: &Packet) -> LMFlowPacket {
    match p.arc_ref() {
        Some(arc) => LMFlowPacket {
            payload: arc.data_ptr(),
            type_id: arc.type_id(),
            timestamp: p.timestamp().0,
            owner: Arc::as_ptr(arc) as *mut c_void,
            drop_fn: None,
        },
        None => LMFlowPacket {
            timestamp: p.timestamp().0,
            ..Default::default()
        },
    }
}

/// **移交**形态:把一份引用交给调用方,调用方须 emit/send 或 lmflow_packet_drop。
pub fn own_packet(p: Packet) -> LMFlowPacket {
    let ts = p.timestamp().0;
    match p.into_arc() {
        Some(arc) => {
            let payload = arc.data_ptr();
            let type_id = arc.type_id();
            LMFlowPacket {
                payload,
                type_id,
                timestamp: ts,
                owner: Arc::into_raw(arc) as *mut c_void,
                drop_fn: None,
            }
        }
        None => LMFlowPacket {
            timestamp: ts,
            ..Default::default()
        },
    }
}

/// 接管一个跨界包(消耗调用方的那份所有权)。
///
/// # Safety
/// `fp` 必须是调用方拥有的包(owner 非空,或 payload+drop_fn 的自建包)。
pub unsafe fn take_packet(fp: LMFlowPacket) -> Packet {
    let ts = Timestamp(fp.timestamp);
    if !fp.owner.is_null() {
        let arc = Arc::from_raw(fp.owner as *const Payload);
        Packet::from_arc(arc, ts)
    } else if !fp.payload.is_null() {
        Packet::from_foreign(fp.payload, fp.type_id, fp.drop_fn).at(ts)
    } else {
        Packet::empty().at(ts)
    }
}

#[no_mangle]
pub extern "C" fn lmflow_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
pub extern "C" fn lmflow_last_error() -> *const c_char {
    last_error::get()
}

#[no_mangle]
pub extern "C" fn lmflow_set_log_callback(
    cb: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
    user: *mut c_void,
) {
    guard_val((), || runtime::set_log_callback(cb, user));
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_drop(pkt: *mut LMFlowPacket) {
    guard_val((), || {
        if pkt.is_null() {
            return;
        }
        let p = &mut *pkt;
        if !p.owner.is_null() {
            drop(Arc::from_raw(p.owner as *const Payload));
        } else if !p.payload.is_null() {
            if let Some(f) = p.drop_fn {
                f(p.payload);
            }
        }
        *p = LMFlowPacket::default();
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_clone(pkt: *const LMFlowPacket) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        if pkt.is_null() {
            return LMFlowPacket::default();
        }
        let src = &*pkt;
        if src.owner.is_null() {
            // 自建包尚未进入引擎,无引用计数可增 —— 不支持克隆
            last_error::set(
                "lmflow_packet_clone can only be used on engine-held packets (owner non-null)",
            );
            return LMFlowPacket::default();
        }
        let arc = Arc::from_raw(src.owner as *const Payload);
        let cloned = arc.clone();
        // 原来的那份引用仍属调用方,不能在此释放
        let _ = Arc::into_raw(arc);
        LMFlowPacket {
            payload: cloned.data_ptr(),
            type_id: cloned.type_id(),
            timestamp: src.timestamp,
            owner: Arc::into_raw(cloned) as *mut c_void,
            drop_fn: None,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_debug_string(pkt: *const LMFlowPacket) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        thread_local! {
            static BUF: std::cell::RefCell<std::ffi::CString> =
                std::cell::RefCell::new(std::ffi::CString::default());
        }
        let s = if pkt.is_null() {
            "Packet{null}".to_string()
        } else {
            let p = &*pkt;
            let ty = if p.payload.is_null() {
                "Empty".to_string()
            } else {
                format!("type#{}", p.type_id)
            };
            format!("Packet{{type={ty}, ts={}}}", Timestamp(p.timestamp))
        };
        BUF.with(|b| {
            *b.borrow_mut() = std::ffi::CString::new(s).unwrap_or_default();
            b.borrow().as_ptr()
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_register_type_name(type_id: u64, name: *const c_char) -> i32 {
    guard(|| {
        let Some(n) = cstr(name) else {
            return fail(Error::InvalidArg("type name is empty or not UTF-8".into()));
        };
        packet::register_type_name(type_id, n);
        code::OK
    })
}

#[no_mangle]
pub extern "C" fn lmflow_type_name(type_id: u64) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        static A: std::sync::LazyLock<runtime::CStrArena> =
            std::sync::LazyLock::new(runtime::CStrArena::default);
        A.intern(&packet::type_name(type_id))
    })
}

// ---------------------------------------------------------------- 内建类型

fn own_builtin(b: Builtin, ts: i64) -> LMFlowPacket {
    own_packet(Packet::from_builtin(b).at(Timestamp(ts)))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_from_bytes(
    data: *const c_void,
    len: usize,
    ts: i64,
) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        let v = if data.is_null() || len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const u8, len).to_vec()
        };
        own_builtin(Builtin::Bytes(v), ts)
    })
}

#[no_mangle]
pub extern "C" fn lmflow_packet_from_i64(value: i64, ts: i64) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        own_builtin(Builtin::I64(value), ts)
    })
}

#[no_mangle]
pub extern "C" fn lmflow_packet_from_f64(value: f64, ts: i64) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        own_builtin(Builtin::F64(value), ts)
    })
}

#[no_mangle]
pub extern "C" fn lmflow_packet_from_bool(value: bool, ts: i64) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        own_builtin(Builtin::Bool(value), ts)
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_from_str(utf8: *const c_char, ts: i64) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        let s = cstr(utf8).unwrap_or("");
        own_builtin(
            Builtin::Str(std::ffi::CString::new(s).unwrap_or_default()),
            ts,
        )
    })
}

/// 借用形态的包 → 只读访问其内建 payload。
unsafe fn peek_builtin<'a>(pkt: *const LMFlowPacket) -> Option<&'a Builtin> {
    if pkt.is_null() {
        return None;
    }
    let p = &*pkt;
    if p.owner.is_null() {
        return None;
    }
    match &*(p.owner as *const Payload) {
        Payload::Builtin(b) => Some(b),
        _ => None,
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_as_bytes(
    pkt: *const LMFlowPacket,
    data: *mut *const c_void,
    len: *mut usize,
) -> bool {
    guard_val(false, || match peek_builtin(pkt) {
        Some(Builtin::Bytes(v)) => {
            if !data.is_null() {
                *data = v.as_ptr() as *const c_void;
            }
            if !len.is_null() {
                *len = v.len();
            }
            true
        }
        _ => false,
    })
}

macro_rules! as_scalar {
    ($name:ident, $variant:ident, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(pkt: *const LMFlowPacket, out: *mut $ty) -> bool {
            guard_val(false, || match peek_builtin(pkt) {
                Some(Builtin::$variant(v)) => {
                    if !out.is_null() {
                        *out = *v;
                    }
                    true
                }
                _ => false,
            })
        }
    };
}
as_scalar!(lmflow_packet_as_i64, I64, i64);
as_scalar!(lmflow_packet_as_f64, F64, f64);
as_scalar!(lmflow_packet_as_bool, Bool, bool);

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_as_str(
    pkt: *const LMFlowPacket,
    out: *mut *const c_char,
) -> bool {
    guard_val(false, || match peek_builtin(pkt) {
        Some(Builtin::Str(s)) => {
            if !out.is_null() {
                *out = s.as_ptr();
            }
            true
        }
        _ => false,
    })
}

// ---------------------------------------------------------------- LMFlowBuffer

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LMFlowBuffer {
    pub data: *mut c_void,
    pub shape: [i64; packet::MAX_DIMS],
    pub strides: [i64; packet::MAX_DIMS],
    pub ndim: i32,
    pub dtype: i32,
    pub flags: u32,
    pub device: i32,
    pub reserved: [i64; 2],
}

impl Default for LMFlowBuffer {
    fn default() -> Self {
        Self {
            data: std::ptr::null_mut(),
            shape: [0; packet::MAX_DIMS],
            strides: [0; packet::MAX_DIMS],
            ndim: 0,
            dtype: 0,
            flags: 0,
            device: 0,
            reserved: [0; 2],
        }
    }
}

pub const BUF_FLAG_READONLY: u32 = 1;

fn fill_buffer(out: *mut LMFlowBuffer, b: &BufferData, readonly: bool, data: *mut c_void) {
    if out.is_null() {
        return;
    }
    let v = LMFlowBuffer {
        data,
        shape: b.shape,
        strides: b.strides,
        ndim: b.ndim,
        dtype: b.dtype,
        flags: if readonly { BUF_FLAG_READONLY } else { 0 },
        device: 0,
        reserved: [0; 2],
    };
    unsafe { std::ptr::write(out, v) };
}

#[no_mangle]
pub extern "C" fn lmflow_dtype_size(dt: i32) -> usize {
    packet::dtype_size(dt)
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_new_buffer(
    ndim: i32,
    shape: *const i64,
    dtype: i32,
    ts: i64,
    out: *mut LMFlowBuffer,
) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        if ndim <= 0 || shape.is_null() {
            last_error::set("lmflow_packet_new_buffer: ndim must be positive and shape non-null");
            return LMFlowPacket::default();
        }
        let dims = std::slice::from_raw_parts(shape, ndim as usize);
        match BufferData::new(dims, dtype) {
            Ok(b) => {
                let pkt = Packet::from_builtin(Builtin::Buffer(b)).at(Timestamp(ts));
                let fp = own_packet(pkt);
                // 填可写视图:此时引擎持有的那份就是调用方拿到的那份(独占)
                if let Payload::Builtin(Builtin::Buffer(b)) = &*(fp.owner as *const Payload) {
                    fill_buffer(out, b, false, b.bytes.as_ptr() as *mut c_void);
                }
                fp
            }
            Err(e) => {
                last_error::set(&e.to_string());
                LMFlowPacket::default()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_from_buffer(
    src: *const LMFlowBuffer,
    ts: i64,
) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        if src.is_null() {
            return LMFlowPacket::default();
        }
        let s = &*src;
        let dims = &s.shape[..s.ndim.max(0) as usize];
        let Ok(mut b) = BufferData::new(dims, s.dtype) else {
            last_error::set("lmflow_packet_from_buffer: invalid shape/dtype");
            return LMFlowPacket::default();
        };
        // 拷进一份行优先连续的缓冲,支持**任意 strides** —— 转置、带步长切片、
        // 甚至负步长的 numpy 视图都要拷对(否则静默数据损坏)。
        if !s.data.is_null() {
            let esz = packet::dtype_size(s.dtype);
            let ndim = dims.len();
            if ndim >= 1 && esz > 0 {
                let last = *dims.last().unwrap();
                let last_stride = s.strides[ndim - 1];
                let row_bytes = (last as usize) * esz;
                let n_rows: i64 = dims[..ndim - 1].iter().product::<i64>().max(1);
                let src_base = s.data as *const u8;
                let dst_base = b.bytes.as_mut_ptr();
                // 里程表遍历外层维度索引,按完整 strides 求每行源偏移。
                let mut idx = vec![0i64; ndim - 1];
                for r in 0..n_rows {
                    let mut so: i64 = 0;
                    for (d, &ix) in idx.iter().enumerate() {
                        so += ix * s.strides[d];
                    }
                    let dofs = (r as usize) * row_bytes;
                    if dofs + row_bytes <= b.bytes.len() {
                        if last_stride == esz as i64 {
                            // 最后一维连续:整行拷
                            std::ptr::copy_nonoverlapping(
                                src_base.offset(so as isize),
                                dst_base.add(dofs),
                                row_bytes,
                            );
                        } else {
                            // 最后一维也跳跃(转置/步长切片/负步长):逐元素拷
                            for k in 0..last {
                                std::ptr::copy_nonoverlapping(
                                    src_base.offset((so + k * last_stride) as isize),
                                    dst_base.add(dofs + (k as usize) * esz),
                                    esz,
                                );
                            }
                        }
                    }
                    // 里程表 +1(最右外层维先进位)
                    for d in (0..ndim - 1).rev() {
                        idx[d] += 1;
                        if idx[d] < dims[d] {
                            break;
                        }
                        idx[d] = 0;
                    }
                }
            }
        }
        own_packet(Packet::from_builtin(Builtin::Buffer(b)).at(Timestamp(ts)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_as_buffer(
    pkt: *const LMFlowPacket,
    out: *mut LMFlowBuffer,
) -> bool {
    guard_val(false, || match peek_builtin(pkt) {
        Some(Builtin::Buffer(b)) => {
            fill_buffer(out, b, true, b.bytes.as_ptr() as *mut c_void);
            true
        }
        _ => false,
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_make_mutable_buffer(
    pkt: *mut LMFlowPacket,
    out: *mut LMFlowBuffer,
) -> i32 {
    guard(|| {
        if pkt.is_null() {
            return fail(Error::InvalidArg("pkt is null".into()));
        }
        let fp = &mut *pkt;
        if fp.owner.is_null() {
            return fail(Error::InvalidArg(
                "make_mutable requires a caller-owned packet (owner non-null); for a borrowed input packet, take_input first".into(),
            ));
        }
        // 取回所有权 → CoW → 再交还
        let mut p = take_packet(*fp);
        let r = match p.make_mutable_builtin() {
            Ok(Builtin::Buffer(b)) => {
                let data = b.bytes.as_mut_ptr() as *mut c_void;
                let snapshot = b.clone();
                fill_buffer(out, &snapshot, false, data);
                code::OK
            }
            Ok(_) => fail(Error::InvalidArg(
                "this packet is not an LMFlowBuffer".into(),
            )),
            Err(e) => fail(e),
        };
        *fp = own_packet(p);
        r
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_make_mutable_bytes(
    pkt: *mut LMFlowPacket,
    data: *mut *mut c_void,
    len: *mut usize,
) -> i32 {
    guard(|| {
        if pkt.is_null() {
            return fail(Error::InvalidArg("pkt is null".into()));
        }
        let fp = &mut *pkt;
        if fp.owner.is_null() {
            return fail(Error::InvalidArg(
                "make_mutable requires a caller-owned packet".into(),
            ));
        }
        let mut p = take_packet(*fp);
        let r = match p.make_mutable_builtin() {
            Ok(Builtin::Bytes(v)) => {
                if !data.is_null() {
                    *data = v.as_mut_ptr() as *mut c_void;
                }
                if !len.is_null() {
                    *len = v.len();
                }
                code::OK
            }
            Ok(_) => fail(Error::InvalidArg("this packet is not BYTES".into())),
            Err(e) => fail(e),
        };
        *fp = own_packet(p);
        r
    })
}

// ---------------------------------------------------------------- 算子注册

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LMFlowKernelVTable {
    pub create: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    pub get_contract: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContract)>,
    pub open: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
    pub process: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
    pub close: Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_register_kernel(
    name: *const c_char,
    vt: *const LMFlowKernelVTable,
    factory: *mut c_void,
) -> i32 {
    guard(|| {
        let Some(n) = cstr(name) else {
            return fail(Error::InvalidArg(
                "kernel name is empty or not UTF-8".into(),
            ));
        };
        if vt.is_null() {
            return fail(Error::InvalidArg(format!("kernel `{n}` vtable is null")));
        }
        let v = &*vt;
        // LMFlowKernelVTable 与 KernelVTable 布局相同,只是 ctx 参数的具体类型不同
        // LMFlowKernelVTable 与 KernelVTable 的函数指针 ABI 完全相同,仅上下文参数的
        // 具名类型不同(LMFlowContext*/LMFlowContract* ↔ void*)。显式标注转换目标类型。
        type CtxFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
        type ContractFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
        let converted = KernelVTable {
            create: v.create,
            get_contract: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContract)>,
                Option<ContractFn>,
            >(v.get_contract),
            open: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
                Option<CtxFn>,
            >(v.open),
            process: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
                Option<CtxFn>,
            >(v.process),
            close: std::mem::transmute::<
                Option<unsafe extern "C" fn(*mut c_void, *mut LMFlowContext) -> i32>,
                Option<CtxFn>,
            >(v.close),
            destroy: v.destroy,
        };
        to_status(crate::kernel::register(n, converted, factory))
    })
}

#[no_mangle]
pub extern "C" fn lmflow_registered_kernel_count() -> usize {
    guard_val(0, || crate::kernel::registered_names().len())
}

#[no_mangle]
pub extern "C" fn lmflow_registered_kernel_name(idx: usize) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        static ARENA: std::sync::LazyLock<runtime::CStrArena> =
            std::sync::LazyLock::new(runtime::CStrArena::default);
        match crate::kernel::registered_names().get(idx) {
            Some(n) => ARENA.intern(n),
            None => c"".as_ptr(),
        }
    })
}

// ---------------------------------------------------------------- Contract

#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_num_inputs(c: *const LMFlowContract) -> usize {
    guard_val(0, || {
        contract_mut(c as *mut LMFlowContract).map_or(0, |x| x.inputs.len())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_num_outputs(c: *const LMFlowContract) -> usize {
    guard_val(0, || {
        contract_mut(c as *mut LMFlowContract).map_or(0, |x| x.outputs.len())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_id(
    c: *const LMFlowContract,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.inputs
            .id_by_tag(cstr(tag).unwrap_or(""), index)
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_id(
    c: *const LMFlowContract,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.outputs
            .id_by_tag(cstr(tag).unwrap_or(""), index)
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_index(
    c: *const LMFlowContract,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.inputs
            .index_by_name(cstr(name).unwrap_or(""))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_index(
    c: *const LMFlowContract,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        let Some(x) = contract_mut(c as *mut LMFlowContract) else {
            return INVALID_ID;
        };
        x.outputs
            .index_by_name(cstr(name).unwrap_or(""))
            .unwrap_or(INVALID_ID)
    })
}

/// contract 阶段的端口名指针需要一个稳定住处;用进程级 arena。
fn contract_arena() -> &'static runtime::CStrArena {
    static A: std::sync::LazyLock<runtime::CStrArena> =
        std::sync::LazyLock::new(runtime::CStrArena::default);
    &A
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_name(
    c: *const LMFlowContract,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        contract_mut(c as *mut LMFlowContract)
            .and_then(|x| x.inputs.name(idx).map(|s| contract_arena().intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_name(
    c: *const LMFlowContract,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        contract_mut(c as *mut LMFlowContract)
            .and_then(|x| x.outputs.name(idx).map(|s| contract_arena().intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_set_any(c: *mut LMFlowContract, idx: usize) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.input_types.get_mut(idx) {
                *s = 0;
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_set_any(c: *mut LMFlowContract, idx: usize) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.output_types.get_mut(idx) {
                *s = 0;
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_input_set_type(
    c: *mut LMFlowContract,
    idx: usize,
    type_id: u64,
) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.input_types.get_mut(idx) {
                *s = type_id;
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_output_set_type(
    c: *mut LMFlowContract,
    idx: usize,
    type_id: u64,
) {
    guard_val((), || {
        if let Some(x) = contract_mut(c) {
            if let Some(s) = x.output_types.get_mut(idx) {
                *s = type_id;
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_contract_require_side_packet(
    c: *mut LMFlowContract,
    name: *const c_char,
) {
    guard_val((), || {
        if let (Some(x), Some(n)) = (contract_mut(c), cstr(name)) {
            x.required_side_packets.push(n.to_string());
        }
    });
}

// ---------------------------------------------------------------- Context

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_num_inputs(c: *const LMFlowContext) -> usize {
    guard_val(0, || ctx_ref(c).map_or(0, |x| x.in_ports.len()))
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_num_outputs(c: *const LMFlowContext) -> usize {
    guard_val(0, || ctx_ref(c).map_or(0, |x| x.out_ports.len()))
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_id(
    c: *const LMFlowContext,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.in_ports.id_by_tag(cstr(tag).unwrap_or(""), index))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_output_id(
    c: *const LMFlowContext,
    tag: *const c_char,
    index: usize,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.out_ports.id_by_tag(cstr(tag).unwrap_or(""), index))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_index(
    c: *const LMFlowContext,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.in_ports.index_by_name(cstr(name).unwrap_or("")))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_output_index(
    c: *const LMFlowContext,
    name: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        ctx_ref(c)
            .and_then(|x| x.out_ports.index_by_name(cstr(name).unwrap_or("")))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_name(
    c: *const LMFlowContext,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c)
            .and_then(|x| x.in_ports.name(idx).map(|s| x.intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_output_name(
    c: *const LMFlowContext,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c)
            .and_then(|x| x.out_ports.name(idx).map(|s| x.intern(s)))
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_node_name(c: *const LMFlowContext) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c).map_or(c"".as_ptr(), |x| x.node_name_cstr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_kernel_name(c: *const LMFlowContext) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        ctx_ref(c).map_or(c"".as_ptr(), |x| x.kernel_name_cstr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_log(c: *const LMFlowContext, level: i32, msg: *const c_char) {
    guard_val((), || {
        if let (Some(x), Some(m)) = (ctx_ref(c), cstr(msg)) {
            x.log(level, m);
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_set_error(c: *const LMFlowContext, msg: *const c_char) {
    guard_val((), || {
        // set_error 需要可变访问;上下文在回调期间由算子独占,故此转换安全
        if let (Some(x), Some(m)) = (ctx_mut(c as *mut LMFlowContext), cstr(msg)) {
            x.set_error(m);
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_close_reason(c: *const LMFlowContext) -> i32 {
    guard_val(0, || ctx_ref(c).map_or(0, |x| x.close_reason))
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_counter_add(
    c: *const LMFlowContext,
    name: *const c_char,
    delta: i64,
) {
    guard_val((), || {
        if let (Some(x), Some(n)) = (ctx_ref(c), cstr(name)) {
            x.shared.counter_add(n, delta);
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_is_empty(c: *const LMFlowContext, idx: usize) -> bool {
    guard_val(true, || {
        ctx_ref(c).is_none_or(|x| x.input(idx).is_none_or(|p| p.is_empty()))
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_is_done(c: *const LMFlowContext, idx: usize) -> bool {
    guard_val(false, || {
        ctx_ref(c).is_some_and(|x| x.inputs_done.get(idx).copied().unwrap_or(false))
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input(c: *const LMFlowContext, idx: usize) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || match ctx_ref(c) {
        Some(x) => x.input(idx).map(borrow_packet).unwrap_or_default(),
        None => LMFlowPacket::default(),
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_payload(
    c: *const LMFlowContext,
    idx: usize,
) -> *const c_void {
    guard_val(std::ptr::null(), || {
        ctx_ref(c)
            .and_then(|x| x.input(idx))
            .and_then(|p| p.arc_ref().map(|a| a.data_ptr() as *const c_void))
            .unwrap_or(std::ptr::null())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_input_timestamp(c: *const LMFlowContext) -> i64 {
    guard_val(Timestamp::unset().0, || {
        ctx_ref(c).map_or(Timestamp::unset().0, |x| x.input_ts.0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_take_input(c: *mut LMFlowContext, idx: usize) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || match ctx_mut(c) {
        Some(x) => own_packet(x.take_input(idx)),
        None => LMFlowPacket::default(),
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_emit(c: *mut LMFlowContext, out_idx: usize, pkt: LMFlowPacket) {
    guard_val((), || {
        let p = take_packet(pkt);
        if let Some(x) = ctx_mut(c) {
            if let Err(e) = x.emit(out_idx, p) {
                x.set_error(&e.to_string());
                last_error::set(&e.to_string());
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_forward(c: *mut LMFlowContext, in_idx: usize, out_idx: usize) {
    guard_val((), || {
        if let Some(x) = ctx_mut(c) {
            if let Err(e) = x.forward(in_idx, out_idx) {
                x.set_error(&e.to_string());
                last_error::set(&e.to_string());
            }
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_set_next_ts_bound(
    c: *mut LMFlowContext,
    out_idx: usize,
    bound: i64,
) {
    guard_val((), || {
        if let Some(x) = ctx_mut(c) {
            x.set_next_bound(out_idx, Timestamp(bound));
        }
    });
}

// ---- options ----

macro_rules! opt_scalar {
    ($name:ident, $method:ident, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            c: *const LMFlowContext,
            key: *const c_char,
            def: $ty,
        ) -> $ty {
            guard_val(def, || {
                ctx_ref(c)
                    .and_then(|x| cstr(key).and_then(|k| x.options.$method(k)))
                    .unwrap_or(def)
            })
        }
    };
}
opt_scalar!(lmflow_ctx_option_i64, i64, i64);
opt_scalar!(lmflow_ctx_option_f64, f64, f64);
opt_scalar!(lmflow_ctx_option_bool, bool, bool);

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_has_option(
    c: *const LMFlowContext,
    key: *const c_char,
) -> bool {
    guard_val(false, || {
        ctx_ref(c).is_some_and(|x| cstr(key).is_some_and(|k| x.options.has(k)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_str(
    c: *const LMFlowContext,
    key: *const c_char,
    def: *const c_char,
) -> *const c_char {
    guard_val(def, || {
        ctx_ref(c)
            .and_then(|x| cstr(key).and_then(|k| x.options.str_cstr(k)))
            .unwrap_or(def)
    })
}

macro_rules! require_scalar {
    ($name:ident, $method:ident, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(
            c: *const LMFlowContext,
            key: *const c_char,
            out: *mut $ty,
        ) -> i32 {
            guard(|| {
                let Some(x) = ctx_ref(c) else {
                    return fail(Error::InvalidArg("context is null".into()));
                };
                let Some(k) = cstr(key) else {
                    return fail(Error::InvalidArg("parameter name is null".into()));
                };
                match x.options.$method(k) {
                    Some(v) => {
                        if !out.is_null() {
                            *out = v;
                        }
                        code::OK
                    }
                    None => fail(Error::InvalidArg(format!(
                        "node `{}`: required parameter options.{k} is missing or type mismatch",
                        x.node_name
                    ))),
                }
            })
        }
    };
}
require_scalar!(lmflow_ctx_require_option_i64, i64, i64);
require_scalar!(lmflow_ctx_require_option_f64, f64, f64);
require_scalar!(lmflow_ctx_require_option_bool, bool, bool);

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_require_option_str(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut *const c_char,
) -> i32 {
    guard(|| {
        let Some(x) = ctx_ref(c) else {
            return fail(Error::InvalidArg("context is null".into()));
        };
        let Some(k) = cstr(key) else {
            return fail(Error::InvalidArg("parameter name is null".into()));
        };
        match x.options.str_cstr(k) {
            Some(p) => {
                if !out.is_null() {
                    *out = p;
                }
                code::OK
            }
            None => fail(Error::InvalidArg(format!(
                "node `{}`: required parameter options.{k} is missing or type mismatch",
                x.node_name
            ))),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_count(
    c: *const LMFlowContext,
    key: *const c_char,
) -> usize {
    guard_val(0, || {
        ctx_ref(c).map_or(0, |x| cstr(key).map_or(0, |k| x.options.count(k)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_i64_array(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut i64,
    cap: usize,
) -> usize {
    guard_val(0, || {
        let (Some(x), Some(k)) = (ctx_ref(c), cstr(key)) else {
            return 0;
        };
        let mut tmp = vec![0i64; cap];
        let n = x.options.i64_array(k, &mut tmp);
        if !out.is_null() && cap > 0 {
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), out, cap.min(n));
        }
        n
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_f64_array(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut f64,
    cap: usize,
) -> usize {
    guard_val(0, || {
        let (Some(x), Some(k)) = (ctx_ref(c), cstr(key)) else {
            return 0;
        };
        let mut tmp = vec![0f64; cap];
        let n = x.options.f64_array(k, &mut tmp);
        if !out.is_null() && cap > 0 {
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), out, cap.min(n));
        }
        n
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_option_str_array(
    c: *const LMFlowContext,
    key: *const c_char,
    out: *mut *const c_char,
    cap: usize,
) -> usize {
    guard_val(0, || {
        let (Some(x), Some(k)) = (ctx_ref(c), cstr(key)) else {
            return 0;
        };
        let mut tmp = vec![std::ptr::null(); cap];
        let n = x.options.str_array(k, &mut tmp);
        if !out.is_null() && cap > 0 {
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), out, cap.min(n));
        }
        n
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_options_json(c: *const LMFlowContext) -> *const c_char {
    guard_val(c"{}".as_ptr(), || {
        ctx_ref(c).map_or(c"{}".as_ptr(), |x| x.options.json_cstr())
    })
}

// ---- side packet ----

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_has_side_packet(
    c: *const LMFlowContext,
    name: *const c_char,
) -> bool {
    guard_val(false, || {
        ctx_ref(c).is_some_and(|x| cstr(name).is_some_and(|n| x.side_packets.contains_key(n)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_ctx_side_packet(
    c: *const LMFlowContext,
    name: *const c_char,
) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        ctx_ref(c)
            .and_then(|x| cstr(name).and_then(|n| x.side_packets.get(n)))
            .map(borrow_packet)
            .unwrap_or_default()
    })
}

// ---------------------------------------------------------------- 图

#[no_mangle]
pub extern "C" fn lmflow_graph_new() -> *mut LMFlowGraph {
    guard_val(std::ptr::null_mut(), || {
        // 此处只分配空槽,真正建图在 init_from_yaml。
        // 顺便把错误状态清干净,免得调用方读到上一次遗留的 last_error。
        last_error::set("");
        Box::into_raw(Box::new(GraphSlot::default())) as *mut LMFlowGraph
    })
}

/// `lmflow_graph_new` 先返回一个空槽,`init_from_yaml` 才真正建图。
///
/// 输入/输出句柄(`LMFlowInput*`/`LMFlowPoller*`)**不**由本槽持有 —— 它们是**调用方拥有**的:
/// `lmflow_graph_input`/`lmflow_graph_add_poller` 返回一个独立的 `Box::into_raw` 句柄,
/// 各自持一份 `Arc<GraphInner>`,须由调用方 `lmflow_input_free`/`lmflow_poller_free` 释放。
/// 这样即使先 `lmflow_graph_free` 了图,句柄内存依旧有效(其 Arc 撑着引擎),
/// 之后再用只会得到「图已结束」的错误,而不是 use-after-free。
#[derive(Default)]
pub struct GraphSlot {
    graph: Option<Graph>,
}

unsafe fn slot_mut<'a>(g: *mut LMFlowGraph) -> Option<&'a mut GraphSlot> {
    if g.is_null() {
        None
    } else {
        Some(&mut *(g as *mut GraphSlot))
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_free(g: *mut LMFlowGraph) {
    guard_val((), || {
        if g.is_null() {
            return;
        }
        let slot = Box::from_raw(g as *mut GraphSlot);
        if let Some(gr) = &slot.graph {
            gr.cancel();
            let _ = gr.wait_done();
        }
        drop(slot);
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_init_from_yaml(
    g: *mut LMFlowGraph,
    yaml: *const c_char,
) -> i32 {
    guard(|| {
        let Some(slot) = slot_mut(g) else {
            return fail(Error::InvalidArg("graph handle is null".into()));
        };
        if slot.graph.is_some() {
            return fail(Error::State("graph already initialized".into()));
        }
        let Some(text) = cstr(yaml) else {
            return fail(Error::InvalidArg("yaml is empty or not UTF-8".into()));
        };
        match Graph::from_yaml(text) {
            Ok(gr) => {
                slot.graph = Some(gr);
                code::OK
            }
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_init_from_yaml_file(
    g: *mut LMFlowGraph,
    path: *const c_char,
) -> i32 {
    guard(|| {
        let Some(slot) = slot_mut(g) else {
            return fail(Error::InvalidArg("graph handle is null".into()));
        };
        let Some(p) = cstr(path) else {
            return fail(Error::InvalidArg("path is null".into()));
        };
        match Graph::from_yaml_file(p) {
            Ok(gr) => {
                slot.graph = Some(gr);
                code::OK
            }
            Err(e) => fail(e),
        }
    })
}

unsafe fn with_graph<F: FnOnce(&Graph) -> i32>(g: *mut LMFlowGraph, f: F) -> i32 {
    match slot_mut(g).and_then(|s| s.graph.as_ref()) {
        Some(gr) => f(gr),
        None => fail(Error::State("graph not yet initialized".into())),
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_start(g: *mut LMFlowGraph) -> i32 {
    guard(|| with_graph(g, |gr| to_status(gr.start())))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_set_side_packet(
    g: *mut LMFlowGraph,
    name: *const c_char,
    pkt: LMFlowPacket,
) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        let Some(n) = cstr(name) else {
            return fail(Error::InvalidArg("side packet name is null".into()));
        };
        with_graph(g, |gr| to_status(gr.set_side_packet(n, p)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_input(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> *mut LMFlowInput {
    guard_val(std::ptr::null_mut(), || {
        let Some(slot) = slot_mut(g) else {
            last_error::set("graph handle is null");
            return std::ptr::null_mut();
        };
        let Some(gr) = slot.graph.as_ref() else {
            last_error::set("graph not yet initialized");
            return std::ptr::null_mut();
        };
        let Some(name) = cstr(port) else {
            last_error::set("port name is null");
            return std::ptr::null_mut();
        };
        let inner = gr.inner().clone();
        match inner.input_edge_by_name(name) {
            Some(edge) => {
                // 调用方拥有:独立 Box,持一份 Arc<GraphInner>。须 lmflow_input_free 释放。
                Box::into_raw(Box::new(InputHandle { graph: inner, edge })) as *mut LMFlowInput
            }
            None => {
                last_error::set(&format!("graph input port `{name}` does not exist"));
                std::ptr::null_mut()
            }
        }
    })
}

unsafe fn input_ref<'a>(i: *mut LMFlowInput) -> Option<&'a InputHandle> {
    if i.is_null() {
        None
    } else {
        Some(&*(i as *const InputHandle))
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_send(i: *mut LMFlowInput, pkt: LMFlowPacket) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        match input_ref(i) {
            Some(h) => to_status(h.graph.send_by_edge(h.edge, p, true)),
            None => fail(Error::InvalidArg("input handle is null".into())),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_try_send(i: *mut LMFlowInput, pkt: LMFlowPacket) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        match input_ref(i) {
            Some(h) => to_status(h.graph.send_by_edge(h.edge, p, false)),
            None => fail(Error::InvalidArg("input handle is null".into())),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_close(i: *mut LMFlowInput) {
    guard_val((), || {
        if let Some(h) = input_ref(i) {
            h.graph.close_edge_pub(h.edge);
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_input_free(i: *mut LMFlowInput) {
    guard_val((), || {
        if !i.is_null() {
            // 调用方拥有:归还这份句柄(及其对引擎的 Arc)。图可能已 free,但句柄仍安全。
            drop(Box::from_raw(i as *mut InputHandle));
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_add_packet(
    g: *mut LMFlowGraph,
    port: *const c_char,
    pkt: LMFlowPacket,
) -> i32 {
    guard(|| {
        let p = take_packet(pkt);
        let Some(name) = cstr(port) else {
            return fail(Error::InvalidArg("port name is null".into()));
        };
        with_graph(g, |gr| {
            let inner = gr.inner();
            match inner.input_edge_by_name(name) {
                Some(e) => to_status(inner.send_by_edge(e, p, true)),
                None => fail(Error::NotFound(format!(
                    "graph input port `{name}` does not exist"
                ))),
            }
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_close_input(g: *mut LMFlowGraph, port: *const c_char) -> i32 {
    guard(|| {
        let Some(name) = cstr(port) else {
            return fail(Error::InvalidArg("port name is null".into()));
        };
        with_graph(g, |gr| to_status(gr.close_input(name)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_close_all_inputs(g: *mut LMFlowGraph) {
    guard_val((), || {
        let _ = with_graph(g, |gr| {
            gr.close_all_inputs();
            code::OK
        });
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_add_poller(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> *mut LMFlowPoller {
    lmflow_graph_add_poller_ex(g, port, false)
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_add_poller_ex(
    g: *mut LMFlowGraph,
    port: *const c_char,
    observe_timestamp_bounds: bool,
) -> *mut LMFlowPoller {
    guard_val(std::ptr::null_mut(), || {
        if observe_timestamp_bounds {
            last_error::set("observe_timestamp_bounds belongs to a later phase, not implemented in this version");
            return std::ptr::null_mut();
        }
        let Some(slot) = slot_mut(g) else {
            last_error::set("graph handle is null");
            return std::ptr::null_mut();
        };
        let Some(gr) = slot.graph.as_ref() else {
            last_error::set("graph not yet initialized");
            return std::ptr::null_mut();
        };
        let Some(name) = cstr(port) else {
            last_error::set("port name is null");
            return std::ptr::null_mut();
        };
        match gr.add_poller(name) {
            Ok(p) => {
                // 调用方拥有:独立 Box,持一份 Arc<GraphInner>。须 lmflow_poller_free 释放。
                Box::into_raw(Box::new(p)) as *mut LMFlowPoller
            }
            Err(e) => {
                last_error::set(&e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

unsafe fn poller_ref<'a>(p: *mut LMFlowPoller) -> Option<&'a Poller> {
    if p.is_null() {
        None
    } else {
        Some(&*(p as *const Poller))
    }
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_next(p: *mut LMFlowPoller, out: *mut LMFlowPacket) -> bool {
    guard_val(false, || match poller_ref(p) {
        Some(poller) => match poller.next() {
            Some(pkt) => {
                if !out.is_null() {
                    *out = own_packet(pkt);
                }
                true
            }
            None => false,
        },
        None => false,
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_try_next(
    p: *mut LMFlowPoller,
    out: *mut LMFlowPacket,
) -> bool {
    guard_val(false, || match poller_ref(p).and_then(|x| x.try_next()) {
        Some(pkt) => {
            if !out.is_null() {
                *out = own_packet(pkt);
            }
            true
        }
        None => false,
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_next_timeout(
    p: *mut LMFlowPoller,
    out: *mut LMFlowPacket,
    timeout_ms: i64,
) -> i32 {
    guard(|| {
        let Some(poller) = poller_ref(p) else {
            return fail(Error::InvalidArg("poller handle is null".into()));
        };
        match poller.next_timeout(std::time::Duration::from_millis(timeout_ms.max(0) as u64)) {
            Ok(Some(pkt)) => {
                if !out.is_null() {
                    *out = own_packet(pkt);
                }
                code::OK
            }
            Ok(None) => code::CLOSED,
            Err(e) => fail(e),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_poller_free(p: *mut LMFlowPoller) {
    guard_val((), || {
        if !p.is_null() {
            // 调用方拥有:归还这份句柄(及其对引擎的 Arc)。图可能已 free,但句柄仍安全。
            drop(Box::from_raw(p as *mut Poller));
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_observe(
    g: *mut LMFlowGraph,
    port: *const c_char,
    cb: Option<unsafe extern "C" fn(*mut c_void, LMFlowPacket)>,
    user: *mut c_void,
) -> i32 {
    lmflow_graph_observe_ex(g, port, false, cb, user)
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_observe_ex(
    g: *mut LMFlowGraph,
    port: *const c_char,
    observe_timestamp_bounds: bool,
    cb: Option<unsafe extern "C" fn(*mut c_void, LMFlowPacket)>,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        if observe_timestamp_bounds {
            return fail(Error::Unsupported(
                "observe_timestamp_bounds belongs to a later phase".into(),
            ));
        }
        let Some(f) = cb else {
            return fail(Error::InvalidArg("callback is null".into()));
        };
        let Some(name) = cstr(port) else {
            return fail(Error::InvalidArg("port name is null".into()));
        };
        with_graph(g, |gr| to_status(gr.inner().add_observer(name, f, user)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_cancel(g: *mut LMFlowGraph) {
    guard_val((), || {
        let _ = with_graph(g, |gr| {
            gr.cancel();
            code::OK
        });
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_done(g: *mut LMFlowGraph) -> i32 {
    guard(|| with_graph(g, |gr| to_status(gr.wait_done())))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_done_timeout(g: *mut LMFlowGraph, ms: i64) -> i32 {
    guard(|| {
        with_graph(g, |gr| {
            to_status(gr.wait_done_timeout(std::time::Duration::from_millis(ms.max(0) as u64)))
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_until_idle(g: *mut LMFlowGraph) -> i32 {
    guard(|| with_graph(g, |gr| to_status(gr.wait_until_idle())))
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_wait_until_idle_timeout(g: *mut LMFlowGraph, ms: i64) -> i32 {
    guard(|| {
        with_graph(g, |gr| {
            to_status(
                gr.wait_until_idle_timeout(std::time::Duration::from_millis(ms.max(0) as u64)),
            )
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_pause(g: *mut LMFlowGraph) {
    guard_val((), || {
        if let Some(gr) = graph_of(g) {
            gr.pause();
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_resume(g: *mut LMFlowGraph) {
    guard_val((), || {
        if let Some(gr) = graph_of(g) {
            gr.resume();
        }
    });
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_last_error(g: *mut LMFlowGraph) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        match slot_mut(g).and_then(|s| s.graph.as_ref()) {
            Some(gr) => gr.inner().shared.error_cstr(),
            None => c"".as_ptr(),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_state(g: *mut LMFlowGraph) -> i32 {
    guard_val(0, || match slot_mut(g).and_then(|s| s.graph.as_ref()) {
        Some(gr) => gr.state() as i32,
        None => State::Created as i32,
    })
}

// ---- 内省 ----

unsafe fn graph_of<'a>(g: *mut LMFlowGraph) -> Option<&'a Graph> {
    slot_mut(g).and_then(|s| s.graph.as_ref())
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_total_queued(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.total_queued())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_total_queued_bytes(g: *mut LMFlowGraph) -> u64 {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.total_queued_bytes())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_queue_depth(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        graph_of(g)
            .and_then(|gr| cstr(port).and_then(|p| gr.inner().queue_depth_by_name(p)))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_dropped_count(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> u64 {
    guard_val(0, || {
        graph_of(g)
            .and_then(|gr| cstr(port).and_then(|p| gr.inner().dropped_by_name(p)))
            .unwrap_or(0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_input_ports(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().num_input_ports())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_output_ports(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().num_output_ports())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_nodes(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || graph_of(g).map_or(0, |gr| gr.inner().nodes_len()))
}

fn graph_arena() -> &'static runtime::CStrArena {
    static A: std::sync::LazyLock<runtime::CStrArena> =
        std::sync::LazyLock::new(runtime::CStrArena::default);
    &A
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_input_port_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .input_port_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_output_port_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .output_port_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_name(g: *mut LMFlowGraph, idx: usize) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .node_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_dump(g: *mut LMFlowGraph) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        thread_local! {
            static BUF: std::cell::RefCell<std::ffi::CString> =
                std::cell::RefCell::new(std::ffi::CString::default());
        }
        let text = graph_of(g).map_or_else(|| "(uninitialized)".to_string(), |gr| gr.dump());
        BUF.with(|b| {
            *b.borrow_mut() = std::ffi::CString::new(text).unwrap_or_default();
            b.borrow().as_ptr()
        })
    })
}

#[repr(C)]
pub struct LMFlowNodeStats {
    pub struct_size: u32,
    pub reserved0: u32,
    pub node_name: *const c_char,
    pub kernel_name: *const c_char,
    pub running: bool,
    pub running_for_us: i64,
    pub processed: u64,
    pub errors: u64,
    pub total_process_us: i64,
    pub max_process_us: i64,
    pub queued: usize,
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_stats(
    g: *mut LMFlowGraph,
    idx: usize,
    out: *mut LMFlowNodeStats,
) -> bool {
    guard_val(false, || {
        if out.is_null() {
            return false;
        }
        let declared = (*out).struct_size as usize;
        if declared < std::mem::size_of::<LMFlowNodeStats>() {
            last_error::set(
                "LMFlowNodeStats.struct_size too small -- set it to sizeof(LMFlowNodeStats)",
            );
            return false;
        }
        let Some(gr) = graph_of(g) else { return false };
        let Some(s) = gr.node_stats(idx) else {
            return false;
        };
        std::ptr::write(
            out,
            LMFlowNodeStats {
                struct_size: std::mem::size_of::<LMFlowNodeStats>() as u32,
                reserved0: 0,
                node_name: graph_arena().intern(&s.node_name),
                kernel_name: graph_arena().intern(&s.kernel_name),
                running: s.running,
                running_for_us: s.running_for_us,
                processed: s.processed,
                errors: s.errors,
                total_process_us: s.total_process_us,
                max_process_us: s.max_process_us,
                queued: s.queued,
            },
        );
        true
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_value(
    g: *mut LMFlowGraph,
    name: *const c_char,
) -> i64 {
    guard_val(0, || {
        graph_of(g)
            .and_then(|gr| cstr(name).map(|n| gr.inner().shared.counter_value(n)))
            .unwrap_or(0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_count(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.counter_names().len())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .shared
                    .counter_names()
                    .get(idx)
                    .map(|n| graph_arena().intern(n))
            })
            .unwrap_or(c"".as_ptr())
    })
}
