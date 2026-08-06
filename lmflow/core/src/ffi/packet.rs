//! C ABI:Packet 跨界表示、内建类型构造/读取、LMFlowBuffer
//!
//! 由 [`super`](../index.html) 的分节拆出 —— 见 `ffi/mod.rs` 的模块头注释,
//! 那里定义了整层的约定(catch_unwind 包裹、last_error、空指针检查、布局钉死)。
#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void};
use std::sync::Arc;

use crate::packet::{self, BufferData, BufferView, Builtin, ExternalBuffer, Packet, Payload};
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
pub unsafe extern "C" fn lmflow_type_id(name: *const c_char) -> u64 {
    guard_val(0, || {
        let Some(name) = cstr(name) else {
            last_error::set("type name is empty or not UTF-8");
            return 0;
        };
        if name.is_empty() {
            last_error::set("type name must not be empty");
            return 0;
        }
        packet::fnv1a_type_id(name)
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
pub const DEVICE_CPU: i32 = 0;
const BUF_FLAG_MASK: u32 = BUF_FLAG_READONLY;

struct ValidatedBuffer<'a> {
    dims: &'a [i64],
    strides: &'a [i64],
    element_size: usize,
    element_count: usize,
    byte_size: u64,
}

fn validate_buffer_descriptor(src: &LMFlowBuffer) -> crate::status::Result<ValidatedBuffer<'_>> {
    let ndim = usize::try_from(src.ndim).map_err(|_| {
        Error::InvalidArg(format!(
            "buffer ndim must be in 1..={}, got {}",
            packet::MAX_DIMS,
            src.ndim
        ))
    })?;
    if !(1..=packet::MAX_DIMS).contains(&ndim) {
        return Err(Error::InvalidArg(format!(
            "buffer ndim must be in 1..={}, got {}",
            packet::MAX_DIMS,
            src.ndim
        )));
    }
    if src.device != DEVICE_CPU {
        return Err(Error::InvalidArg(format!(
            "buffer device {} is not supported; only LMFLOW_DEVICE_CPU (0) is available",
            src.device
        )));
    }
    if src.flags & !BUF_FLAG_MASK != 0 {
        return Err(Error::InvalidArg(format!(
            "buffer flags contain unknown bits: 0x{:x}",
            src.flags & !BUF_FLAG_MASK
        )));
    }
    if src.reserved != [0; 2] {
        return Err(Error::InvalidArg(
            "buffer reserved fields must be zero".into(),
        ));
    }
    if src.shape[ndim..].iter().any(|&value| value != 0) {
        return Err(Error::InvalidArg(
            "buffer shape entries after ndim must be zero".into(),
        ));
    }
    if src.strides[ndim..].iter().any(|&value| value != 0) {
        return Err(Error::InvalidArg(
            "buffer stride entries after ndim must be zero".into(),
        ));
    }

    let dims = &src.shape[..ndim];
    let strides = &src.strides[..ndim];
    let element_size = packet::dtype_size(src.dtype);
    if element_size == 0 {
        return Err(Error::InvalidArg(format!(
            "buffer dtype {} is unknown",
            src.dtype
        )));
    }

    let mut element_count = 1usize;
    let mut min_offset = 0i128;
    let mut max_offset = 0i128;
    for (axis, (&dim, &stride)) in dims.iter().zip(strides).enumerate() {
        if dim < 0 {
            return Err(Error::InvalidArg(format!(
                "buffer shape[{axis}] must not be negative, got {dim}"
            )));
        }
        let dim = usize::try_from(dim)
            .map_err(|_| Error::InvalidArg(format!("buffer shape[{axis}] is too large")))?;
        element_count = element_count
            .checked_mul(dim)
            .ok_or_else(|| Error::InvalidArg("buffer element count overflow".into()))?;
        let extent = (dim.saturating_sub(1) as i128)
            .checked_mul(i128::from(stride))
            .ok_or_else(|| Error::InvalidArg("buffer stride extent overflow".into()))?;
        if extent < 0 {
            min_offset = min_offset
                .checked_add(extent)
                .ok_or_else(|| Error::InvalidArg("buffer minimum offset overflow".into()))?;
        } else {
            max_offset = max_offset
                .checked_add(extent)
                .ok_or_else(|| Error::InvalidArg("buffer maximum offset overflow".into()))?;
        }
    }
    let last_byte = max_offset
        .checked_add(element_size.saturating_sub(1) as i128)
        .ok_or_else(|| Error::InvalidArg("buffer byte extent overflow".into()))?;
    if min_offset < isize::MIN as i128 || last_byte > isize::MAX as i128 {
        return Err(Error::InvalidArg(format!(
            "buffer address range [{min_offset}, {last_byte}] exceeds platform pointer offsets"
        )));
    }
    if element_count > 0 && src.data.is_null() {
        return Err(Error::InvalidArg(
            "buffer data must be non-null for a non-empty shape".into(),
        ));
    }
    if element_count > 0 {
        let base = src.data as usize as i128;
        let first_address = base
            .checked_add(min_offset)
            .ok_or_else(|| Error::InvalidArg("buffer start address overflow".into()))?;
        let last_address = base
            .checked_add(last_byte)
            .ok_or_else(|| Error::InvalidArg("buffer end address overflow".into()))?;
        if first_address < 0 || last_address > usize::MAX as i128 {
            return Err(Error::InvalidArg(format!(
                "buffer address range [{first_address}, {last_address}] is outside the platform address space"
            )));
        }
    }
    let byte_size = element_count
        .checked_mul(element_size)
        .ok_or_else(|| Error::InvalidArg("buffer logical byte count overflow".into()))?
        .try_into()
        .map_err(|_| Error::InvalidArg("buffer logical byte count exceeds u64".into()))?;

    Ok(ValidatedBuffer {
        dims,
        strides,
        element_size,
        element_count,
        byte_size,
    })
}

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
        device: DEVICE_CPU,
        reserved: [0; 2],
    };
    unsafe { std::ptr::write(out, v) };
}

fn fill_buffer_view(out: *mut LMFlowBuffer, view: BufferView) {
    if out.is_null() {
        return;
    }
    let value = LMFlowBuffer {
        data: view.data,
        shape: view.shape,
        strides: view.strides,
        ndim: view.ndim,
        dtype: view.dtype,
        flags: if view.readonly { BUF_FLAG_READONLY } else { 0 },
        device: DEVICE_CPU,
        reserved: [0; 2],
    };
    unsafe { std::ptr::write(out, value) };
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
        if !(1..=packet::MAX_DIMS as i32).contains(&ndim) || shape.is_null() {
            last_error::set(&format!(
                "lmflow_packet_new_buffer: ndim must be in 1..={}, and shape must be non-null",
                packet::MAX_DIMS
            ));
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
            last_error::set("lmflow_packet_from_buffer: src is null");
            return LMFlowPacket::default();
        }
        let s = &*src;
        let validated = match validate_buffer_descriptor(s) {
            Ok(validated) => validated,
            Err(error) => {
                last_error::set(&format!("lmflow_packet_from_buffer: {error}"));
                return LMFlowPacket::default();
            }
        };
        let dims = validated.dims;
        let Ok(mut b) = BufferData::new(dims, s.dtype) else {
            last_error::set("lmflow_packet_from_buffer: invalid shape/dtype");
            return LMFlowPacket::default();
        };
        // 拷进一份行优先连续的缓冲,支持**任意 strides** —— 转置、带步长切片、
        // 甚至负步长的 numpy 视图都要拷对(否则静默数据损坏)。
        if validated.element_count > 0 {
            let esz = validated.element_size;
            let ndim = dims.len();
            if ndim >= 1 && esz > 0 {
                let last = *dims.last().unwrap();
                let last_stride = validated.strides[ndim - 1];
                let row_bytes = (last as usize)
                    .checked_mul(esz)
                    .expect("validated buffer byte count");
                let n_rows = validated.element_count / last.max(1) as usize;
                let src_base = s.data as *const u8;
                let dst_base = b.bytes.as_mut_ptr();
                // 先判**整块连续**(strides 恰好是紧密行优先布局),一次拷完。
                //
                // 下面的通用路径以「最后一维 = 一行」为单位搬运,而 HWC 图像的最后一维
                // 正是通道数(2/3/4)—— 于是 1920x1080x2 的帧会退化成 200 万次 2 字节
                // 拷贝,实测 6.7ms、约 600MB/s,比 memcpy 慢约 30 倍。而 numpy 传进来的
                // 数组绝大多数是连续的,这条快路径覆盖的正是最常见也最大的那些。
                let packed = {
                    let mut expected = esz as i64;
                    let mut ok = true;
                    for d in (0..ndim).rev() {
                        if validated.strides[d] != expected {
                            ok = false;
                            break;
                        }
                        // dims 的乘积已被 BufferData::new 验证过(它据此分配了 b.bytes),
                        // 故此处不会溢出;仍用 checked 以防将来放宽校验。
                        match expected.checked_mul(dims[d]) {
                            Some(next) => expected = next,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    ok
                };
                if packed {
                    // 与通用路径读取的字节区间完全相同(n_rows * row_bytes == b.bytes.len()),
                    // 故安全性无差别。
                    std::ptr::copy_nonoverlapping(src_base, dst_base, b.bytes.len());
                } else {
                    // 里程表遍历外层维度索引,按完整 strides 求每行源偏移。
                    let mut idx = vec![0i64; ndim - 1];
                    for r in 0..n_rows {
                        let mut source_offset = 0i128;
                        for (d, &ix) in idx.iter().enumerate() {
                            source_offset += i128::from(ix) * i128::from(validated.strides[d]);
                        }
                        let source_offset =
                            isize::try_from(source_offset).expect("validated source offset");
                        let dofs = r * row_bytes;
                        if dofs + row_bytes <= b.bytes.len() {
                            if last_stride == esz as i64 {
                                // 最后一维连续:整行拷
                                std::ptr::copy_nonoverlapping(
                                    src_base.offset(source_offset),
                                    dst_base.add(dofs),
                                    row_bytes,
                                );
                            } else {
                                // 最后一维也跳跃(转置/步长切片/负步长):逐元素拷
                                for k in 0..last {
                                    let element_offset = source_offset as i128
                                        + i128::from(k) * i128::from(last_stride);
                                    std::ptr::copy_nonoverlapping(
                                        src_base.offset(
                                            isize::try_from(element_offset)
                                                .expect("validated element offset"),
                                        ),
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
        }
        own_packet(Packet::from_builtin(Builtin::Buffer(b)).at(Timestamp(ts)))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_adopt_buffer(
    src: *const LMFlowBuffer,
    ts: i64,
    release_fn: Option<unsafe extern "C" fn(*mut c_void)>,
    user_data: *mut c_void,
) -> LMFlowPacket {
    guard_val(LMFlowPacket::default(), || {
        if src.is_null() {
            last_error::set("lmflow_packet_adopt_buffer: src is null");
            return LMFlowPacket::default();
        }
        let Some(release_fn) = release_fn else {
            last_error::set("lmflow_packet_adopt_buffer: release_fn is null");
            return LMFlowPacket::default();
        };
        let source = &*src;
        let validated = match validate_buffer_descriptor(source) {
            Ok(validated) => validated,
            Err(error) => {
                last_error::set(&format!("lmflow_packet_adopt_buffer: {error}"));
                return LMFlowPacket::default();
            }
        };
        let packet = Packet::from_external_buffer(ExternalBuffer {
            data: source.data,
            shape: source.shape,
            strides: source.strides,
            ndim: source.ndim,
            dtype: source.dtype,
            readonly: source.flags & BUF_FLAG_READONLY != 0,
            element_count: validated.element_count,
            byte_size: validated.byte_size,
            release_fn,
            user_data,
        })
        .at(Timestamp(ts));
        own_packet(packet)
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_packet_as_buffer(
    pkt: *const LMFlowPacket,
    out: *mut LMFlowBuffer,
) -> bool {
    guard_val(false, || {
        if pkt.is_null() {
            return false;
        }
        let packet = &*pkt;
        if packet.owner.is_null() {
            return false;
        }
        let payload = &*(packet.owner as *const Payload);
        let Some(view) = payload.buffer_view() else {
            return false;
        };
        fill_buffer_view(out, view);
        true
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
        let r = match p.make_mutable_buffer() {
            Ok(view) => {
                fill_buffer_view(out, view);
                code::OK
            }
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
