//! C ABI:Packet 跨界表示、内建类型构造/读取、LMFlowBuffer
//!
//! 由 [`super`](../index.html) 的分节拆出 —— 见 `ffi/mod.rs` 的模块头注释,
//! 那里定义了整层的约定(catch_unwind 包裹、last_error、空指针检查、布局钉死)。
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void};
use std::sync::Arc;

use crate::packet::{self, BufferData, Builtin, Packet, Payload};
use crate::runtime::{self, last_error};
use crate::status::{code, Error};
use crate::timestamp::Timestamp;

use super::*;

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
pub unsafe extern "C" fn lmflow_register_type_descriptor(
    type_id: u64,
    name: *const c_char,
    size: usize,
    align: usize,
) -> i32 {
    guard(|| {
        let Some(name) = cstr(name) else {
            return fail(Error::InvalidArg("type name is empty or not UTF-8".into()));
        };
        match packet::register_type_descriptor(type_id, name, size, align) {
            Ok(()) => code::OK,
            Err(error) => fail(error),
        }
    })
}

#[no_mangle]
pub extern "C" fn lmflow_type_size(type_id: u64) -> usize {
    guard_val(0, || {
        packet::type_descriptor(type_id).map_or(0, |descriptor| descriptor.size)
    })
}

#[no_mangle]
pub extern "C" fn lmflow_type_align(type_id: u64) -> usize {
    guard_val(0, || {
        packet::type_descriptor(type_id).map_or(0, |descriptor| descriptor.align)
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
