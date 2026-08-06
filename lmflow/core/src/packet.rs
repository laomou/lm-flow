//! 数据包:引用计数共享的不可变 payload + 时间戳。
//!
//! 引擎**不解释** payload 内容,只搬引用、只按 `type_id` 做相等性校验。
//! 三种 payload 形态:
//!   * `Native`  —— Rust 侧构造的任意类型(类型擦除)
//!   * `Builtin` —— 引擎分配的内建类型,跨语言约定布局(标量/字节/字符串/N 维缓冲)
//!   * `Foreign` —— 外部(C/C++)构造:裸指针 + `drop_fn`
//!
//! 内建 payload 支持写时复制(CoW)。显式 adopt 的外部 `BUFFER` 也支持：
//! 独占且可写时原地返回，READONLY 或共享时按描述符复制成引擎缓冲。
//! 普通 `Foreign` 只有 `drop_fn`、无从复制;`Native` 是类型擦除的 `Box<dyn Any>`,同理。

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{c_void, CString};
use std::sync::Arc;

use crate::metadata::{Metadata, MetadataValue};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

#[derive(Clone, Copy, Default)]
pub(crate) struct CowCopyCounters {
    pub copies: u64,
    pub bytes: u64,
}

thread_local! {
    static COW_COPY_SCOPES: RefCell<Vec<CowCopyCounters>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn begin_cow_copy_scope() {
    COW_COPY_SCOPES.with(|scopes| scopes.borrow_mut().push(CowCopyCounters::default()));
}

pub(crate) fn end_cow_copy_scope() -> CowCopyCounters {
    COW_COPY_SCOPES.with(|scopes| scopes.borrow_mut().pop().unwrap_or_default())
}

fn record_cow_copy(bytes: u64) {
    COW_COPY_SCOPES.with(|scopes| {
        let mut scopes = scopes.borrow_mut();
        let Some(counters) = scopes.last_mut() else {
            return;
        };
        counters.copies = counters.copies.saturating_add(1);
        counters.bytes = counters.bytes.saturating_add(bytes);
    });
}

/// 内建类型标识,与 `include/flow.h` 的 `LMFLOW_TYPE_*` 一致。0..15 为内建保留。
pub mod type_id {
    pub const NONE: u64 = 0;
    pub const BYTES: u64 = 1;
    pub const I64: u64 = 2;
    pub const F64: u64 = 3;
    pub const BOOL: u64 = 4;
    pub const STR: u64 = 5;
    pub const BUFFER: u64 = 6;
    pub const HOST_OBJECT: u64 = 7; // 预留,本版本未启用
}

/// `LMFLOW_DTYPE_*`
pub mod dtype {
    pub const U8: i32 = 0;
    pub const I8: i32 = 1;
    pub const U16: i32 = 2;
    pub const I16: i32 = 3;
    pub const I32: i32 = 4;
    pub const I64: i32 = 5;
    pub const F16: i32 = 6;
    pub const F32: i32 = 7;
    pub const F64: i32 = 8;
}

pub const MAX_DIMS: usize = 8;

/// A Rust type with an explicitly declared cross-language representation.
///
/// Implementing this trait promises that values of `Self` have the ABI layout expected by the
/// foreign code that uses [`Self::TYPE_NAME`]. [`Packet::from_interop`] hashes that stable name
/// and stores the value behind a foreign-readable pointer.
///
/// # Safety
///
/// The implementation must guarantee that:
///
/// * `Self` has the same size, alignment and field layout as the foreign type registered under
///   `TYPE_NAME` (normally use `#[repr(C)]`);
/// * foreign readers treat the pointer as immutable and do not retain it beyond the packet's
///   lifetime;
/// * `TYPE_NAME` is stable and uniquely identifies that layout across all participating binaries.
pub unsafe trait InteropType: Any + Send + Sync {
    const TYPE_NAME: &'static str;
}

/// A registered custom type's cross-language identity and fixed payload layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeDescriptor {
    pub name: String,
    pub size: usize,
    pub align: usize,
}

/// 单个元素的字节数;未知 dtype 返回 0。
pub fn dtype_size(dt: i32) -> usize {
    match dt {
        dtype::U8 | dtype::I8 => 1,
        dtype::U16 | dtype::I16 | dtype::F16 => 2,
        dtype::I32 | dtype::F32 => 4,
        dtype::I64 | dtype::F64 => 8,
        _ => 0,
    }
}

/// 计算与 C++ 糖层一致的类型标识。
///
/// `flow.hpp` 里 `lmflow::TypeId<T>()` = `NormalizeTypeId(Fnv1a(typeid(T).name()))`,
/// 参数就是**修饰名**(Itanium ABI 下 `int` 是 `"i"`、`double` 是 `"d"`、
/// `std::string` 是 `"NSt7__cxx1112basic_string..."`)。
///
/// Rust 宿主要送一个 C++ 算子能按类型读取的包时,优先让类型实现 [`InteropType`]
/// 并用 [`Packet::from_interop`];底层 unsafe [`Packet::new_interop`] 可直接接收本函数
/// 算出的 id。C++ 侧用 `LMFLOW_DECLARE_TYPE_NAME` 声明同一个稳定名字即可对齐。
pub fn fnv1a_type_id(mangled_name: &str) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for b in mangled_name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    // 内建类型占 0..15,自定义标识必须避开
    if h < 16 {
        h + 16
    } else {
        h
    }
}

#[derive(Default)]
struct TypeRegistry {
    by_id: BTreeMap<u64, TypeDescriptor>,
    by_name: BTreeMap<String, u64>,
}

static TYPE_REGISTRY: std::sync::LazyLock<std::sync::Mutex<TypeRegistry>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(TypeRegistry::default()));

/// Strictly register a custom type's stable name and ABI layout.
///
/// Re-registering the exact same descriptor is idempotent. Reusing an id or name for a different
/// layout is rejected so a hash collision or inconsistent declaration cannot silently pass the
/// port contract's numeric `type_id` check.
pub fn register_type_descriptor(id: u64, name: &str, size: usize, align: usize) -> Result<()> {
    if id < 16 {
        return Err(Error::InvalidArg(format!(
            "custom type id {id} is reserved; custom ids must be >= 16"
        )));
    }
    if name.is_empty() {
        return Err(Error::InvalidArg(
            "custom type name must not be empty".into(),
        ));
    }
    if size == 0 || align == 0 || !align.is_power_of_two() {
        return Err(Error::InvalidArg(format!(
            "custom type `{name}` has invalid layout: size={size}, align={align}"
        )));
    }
    let expected_id = fnv1a_type_id(name);
    if id != expected_id {
        return Err(Error::InvalidArg(format!(
            "custom type `{name}` must use its stable-name id {expected_id}, got {id}"
        )));
    }

    let descriptor = TypeDescriptor {
        name: name.to_string(),
        size,
        align,
    };
    let mut registry = TYPE_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing_id) = registry.by_name.get(name) {
        if *existing_id != id {
            return Err(Error::InvalidArg(format!(
                "custom type name `{name}` is already registered with id {existing_id}, \
                 cannot reuse it with id {id}"
            )));
        }
    }
    if let Some(existing) = registry.by_id.get(&id) {
        if existing == &descriptor {
            return Ok(());
        }
        return Err(Error::InvalidArg(format!(
            "custom type id {id} is already registered as `{}` (size={}, align={}), \
             cannot register `{name}` (size={size}, align={align})",
            existing.name, existing.size, existing.align
        )));
    }
    registry.by_name.insert(name.to_string(), id);
    registry.by_id.insert(id, descriptor);
    Ok(())
}

pub fn type_descriptor(id: u64) -> Option<TypeDescriptor> {
    TYPE_REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .by_id
        .get(&id)
        .cloned()
}

/// 类型标识的可读名字 —— 让「类型不符」的报错能指出是什么类型,而不是两个数字。
pub fn type_name(id: u64) -> String {
    match id {
        type_id::NONE => "any/undeclared".to_string(),
        type_id::BYTES => "Bytes".to_string(),
        type_id::I64 => "I64".to_string(),
        type_id::F64 => "F64".to_string(),
        type_id::BOOL => "Bool".to_string(),
        type_id::STR => "Str".to_string(),
        type_id::BUFFER => "Buffer".to_string(),
        type_id::HOST_OBJECT => "HostObject".to_string(),
        other => TYPE_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_id
            .get(&other)
            .map(|descriptor| descriptor.name.clone())
            .unwrap_or_else(|| format!("type#{other}")),
    }
}

/// N 维带步长缓冲。引擎分配的缓冲总是连续、行优先。
#[derive(Clone, Debug)]
pub struct BufferData {
    pub bytes: Vec<u8>,
    pub shape: [i64; MAX_DIMS],
    pub strides: [i64; MAX_DIMS],
    pub ndim: i32,
    pub dtype: i32,
}

impl BufferData {
    /// 按 shape/dtype 分配连续缓冲(行优先),内容置零。
    pub fn new(shape: &[i64], dt: i32) -> Result<Self> {
        if shape.is_empty() || shape.len() > MAX_DIMS {
            return Err(Error::InvalidArg(format!(
                "ndim must be in 1..={MAX_DIMS}, got {}",
                shape.len()
            )));
        }
        let esz = dtype_size(dt);
        if esz == 0 {
            return Err(Error::InvalidArg(format!("unknown dtype {dt}")));
        }
        let mut count: i64 = 1;
        for &d in shape {
            if d < 0 {
                return Err(Error::InvalidArg("shape must not be negative".into()));
            }
            count = count
                .checked_mul(d)
                .ok_or_else(|| Error::InvalidArg("shape product overflow".into()))?;
        }
        let count = usize::try_from(count)
            .map_err(|_| Error::InvalidArg("shape product exceeds addressable memory".into()))?;
        let total = count
            .checked_mul(esz)
            .ok_or_else(|| Error::InvalidArg("buffer byte count overflow".into()))?;

        let mut s = [0i64; MAX_DIMS];
        let mut st = [0i64; MAX_DIMS];
        s[..shape.len()].copy_from_slice(shape);
        // 行优先连续:最后一维步长 = 元素大小
        let mut acc = esz as i64;
        for i in (0..shape.len()).rev() {
            st[i] = acc;
            acc = acc
                .checked_mul(shape[i])
                .ok_or_else(|| Error::InvalidArg("buffer stride overflow".into()))?;
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| Error::InvalidArg(format!("cannot allocate {total} buffer bytes")))?;
        bytes.resize(total, 0);
        Ok(Self {
            bytes,
            shape: s,
            strides: st,
            ndim: shape.len() as i32,
            dtype: dt,
        })
    }

    pub fn shape_slice(&self) -> &[i64] {
        &self.shape[..self.ndim as usize]
    }
}

pub(crate) type BufferReleaseFn = unsafe extern "C" fn(*mut c_void);

/// 外部所有权的 CPU buffer。描述符由引擎按值保存，最后一个 Packet 引用释放时回调宿主。
pub(crate) struct ExternalBuffer {
    pub(crate) data: *mut c_void,
    pub(crate) shape: [i64; MAX_DIMS],
    pub(crate) strides: [i64; MAX_DIMS],
    pub(crate) ndim: i32,
    pub(crate) dtype: i32,
    pub(crate) readonly: bool,
    pub(crate) element_count: usize,
    pub(crate) byte_size: u64,
    pub(crate) release_fn: BufferReleaseFn,
    pub(crate) user_data: *mut c_void,
}

impl ExternalBuffer {
    pub fn byte_size(&self) -> u64 {
        self.byte_size
    }

    fn packet_payload_ptr(&self) -> *mut c_void {
        if self.data.is_null() {
            std::ptr::NonNull::<u8>::dangling().as_ptr() as *mut c_void
        } else {
            self.data
        }
    }

    fn copy_to_owned(&self) -> Result<BufferData> {
        let dims = &self.shape[..self.ndim as usize];
        let mut owned = BufferData::new(dims, self.dtype)?;
        if self.element_count == 0 {
            return Ok(owned);
        }

        let element_size = dtype_size(self.dtype);
        let ndim = dims.len();
        let mut index = vec![0i64; ndim];
        for element in 0..self.element_count {
            let offset = index
                .iter()
                .zip(self.strides)
                .take(ndim)
                .try_fold(0i128, |offset, (&coordinate, stride)| {
                    i128::from(coordinate)
                        .checked_mul(i128::from(stride))
                        .and_then(|term| offset.checked_add(term))
                })
                .ok_or_else(|| Error::InvalidArg("buffer element offset overflow".into()))?;
            let offset = isize::try_from(offset)
                .map_err(|_| Error::InvalidArg("buffer element offset overflow".into()))?;
            let source = unsafe { (self.data as *const u8).offset(offset) };
            let destination = unsafe { owned.bytes.as_mut_ptr().add(element * element_size) };
            unsafe { std::ptr::copy_nonoverlapping(source, destination, element_size) };

            for axis in (0..ndim).rev() {
                index[axis] += 1;
                if index[axis] < dims[axis] {
                    break;
                }
                index[axis] = 0;
            }
        }
        Ok(owned)
    }
}

impl Drop for ExternalBuffer {
    fn drop(&mut self) {
        unsafe { (self.release_fn)(self.user_data) };
        self.user_data = std::ptr::null_mut();
        self.data = std::ptr::null_mut();
    }
}

impl std::fmt::Debug for ExternalBuffer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExternalBuffer")
            .field("data", &self.data)
            .field("shape", &&self.shape[..self.ndim as usize])
            .field("strides", &&self.strides[..self.ndim as usize])
            .field("dtype", &self.dtype)
            .field("readonly", &self.readonly)
            .finish()
    }
}

// 安全性:外部宿主把 buffer 所有权移交给数据流时，承诺其内存和 release 回调可跨线程。
unsafe impl Send for ExternalBuffer {}
unsafe impl Sync for ExternalBuffer {}

#[derive(Clone, Copy)]
pub(crate) struct BufferView {
    pub data: *mut c_void,
    pub shape: [i64; MAX_DIMS],
    pub strides: [i64; MAX_DIMS],
    pub ndim: i32,
    pub dtype: i32,
    pub readonly: bool,
}

#[derive(Clone, Debug)]
pub enum Builtin {
    Bytes(Vec<u8>),
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(CString),
    Buffer(BufferData),
}

impl Builtin {
    pub fn type_id(&self) -> u64 {
        match self {
            Builtin::Bytes(_) => type_id::BYTES,
            Builtin::I64(_) => type_id::I64,
            Builtin::F64(_) => type_id::F64,
            Builtin::Bool(_) => type_id::BOOL,
            Builtin::Str(_) => type_id::STR,
            Builtin::Buffer(_) => type_id::BUFFER,
        }
    }

    /// 可计量的字节数(用于全局水位)。标量按其自身大小计。
    pub fn byte_size(&self) -> u64 {
        match self {
            Builtin::Bytes(v) => v.len() as u64,
            Builtin::I64(_) | Builtin::F64(_) => 8,
            Builtin::Bool(_) => 1,
            Builtin::Str(s) => s.as_bytes().len() as u64,
            Builtin::Buffer(b) => b.bytes.len() as u64,
        }
    }

    /// 跨 FFI 暴露的数据指针。
    fn data_ptr(&self) -> *mut c_void {
        match self {
            Builtin::Bytes(v) => v.as_ptr() as *mut c_void,
            Builtin::I64(v) => v as *const i64 as *mut c_void,
            Builtin::F64(v) => v as *const f64 as *mut c_void,
            Builtin::Bool(v) => v as *const bool as *mut c_void,
            Builtin::Str(s) => s.as_ptr() as *mut c_void,
            Builtin::Buffer(b) => b.bytes.as_ptr() as *mut c_void,
        }
    }
}

/// 外部(C/C++)构造的 payload:引擎只持指针,引用归零时回调 `drop_fn`。
pub struct Foreign {
    pub ptr: *mut c_void,
    pub drop_fn: Option<unsafe extern "C" fn(*mut c_void)>,
    pub type_id: u64,
    pub byte_size: u64,
}

impl Drop for Foreign {
    fn drop(&mut self) {
        if let Some(f) = self.drop_fn {
            if !self.ptr.is_null() {
                // 安全性:drop_fn 由创建方提供,契约是「对该指针恰好调用一次」。
                unsafe { f(self.ptr) };
            }
        }
        self.ptr = std::ptr::null_mut();
    }
}

impl std::fmt::Debug for Foreign {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Foreign{{ptr:{:?}, type_id:{}}}", self.ptr, self.type_id)
    }
}

// 安全性断言:payload 在数据流中任一时刻只被单线程访问(由「节点独占令牌」保证,
// 见 docs/design.md §7.0 规则 R3);drop 可能发生在另一线程,故要求 drop_fn 可跨线程调用。
// 这是本 crate 唯一的 Send/Sync unsafe 断言。
unsafe impl Send for Foreign {}
unsafe impl Sync for Foreign {}

pub enum Payload {
    Native(Box<dyn Any + Send + Sync>),
    Builtin(Builtin),
    Foreign(Foreign),
}

impl Payload {
    pub fn type_id(&self) -> u64 {
        match self {
            // Rust 原生 payload 不参与跨语言类型校验(见 Packet::new 文档)
            Payload::Native(value) => {
                if value.is::<ExternalBuffer>() {
                    type_id::BUFFER
                } else {
                    type_id::NONE
                }
            }
            Payload::Builtin(b) => b.type_id(),
            Payload::Foreign(f) => f.type_id,
        }
    }

    pub fn byte_size(&self) -> u64 {
        match self {
            Payload::Native(value) => value
                .downcast_ref::<ExternalBuffer>()
                .map_or(0, ExternalBuffer::byte_size),
            Payload::Foreign(f) => f.byte_size,
            Payload::Builtin(b) => b.byte_size(),
        }
    }

    /// 跨 FFI 暴露的数据指针。
    pub fn data_ptr(&self) -> *mut c_void {
        match self {
            Payload::Native(b) => {
                if let Some(buffer) = b.downcast_ref::<ExternalBuffer>() {
                    return buffer.packet_payload_ptr();
                }
                // 取具体值的地址(瘦指针)。type_id 为 NONE,故 C 侧读取由用户自负。
                let p: *const dyn Any = &**b;
                p as *const () as *mut c_void
            }
            Payload::Builtin(b) => b.data_ptr(),
            Payload::Foreign(f) => f.ptr,
        }
    }

    pub(crate) fn buffer_view(&self) -> Option<BufferView> {
        match self {
            Payload::Builtin(Builtin::Buffer(buffer)) => Some(BufferView {
                data: buffer.bytes.as_ptr() as *mut c_void,
                shape: buffer.shape,
                strides: buffer.strides,
                ndim: buffer.ndim,
                dtype: buffer.dtype,
                readonly: true,
            }),
            Payload::Native(value) => {
                value
                    .downcast_ref::<ExternalBuffer>()
                    .map(|buffer| BufferView {
                        data: buffer.data,
                        shape: buffer.shape,
                        strides: buffer.strides,
                        ndim: buffer.ndim,
                        dtype: buffer.dtype,
                        readonly: true,
                    })
            }
            _ => None,
        }
    }
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Payload::Native(value) => match value.downcast_ref::<ExternalBuffer>() {
                Some(buffer) => write!(f, "{buffer:?}"),
                None => f.write_str("Native"),
            },
            Payload::Builtin(b) => write!(f, "{b:?}"),
            Payload::Foreign(x) => write!(f, "{x:?}"),
        }
    }
}

/// A timestamped data packet.
///
/// The payload is immutable and shared: `Clone` bumps a reference count and **does not copy the
/// data**. Mutation goes through copy-on-write, which is only actually copy-free while this is the
/// sole reference — which is why a kernel should
/// [`take_input`](crate::KernelCtx::take_input) rather than borrow when it intends to modify.
#[derive(Clone)]
pub(crate) struct PacketBody {
    pub payload: Arc<Payload>,
    pub metadata: Arc<Metadata>,
}

static EMPTY_METADATA: std::sync::LazyLock<Metadata> = std::sync::LazyLock::new(Metadata::new);

#[derive(Clone, Default)]
pub struct Packet {
    data: Option<Arc<PacketBody>>,
    ts: Timestamp,
}

impl Packet {
    /// 空包:只带时间戳,用于时间戳边界推进与关流。
    pub fn empty() -> Self {
        Self::default()
    }

    /// A Rust-native value.
    ///
    /// **The resulting `type_id` is `NONE`, so this packet cannot satisfy a typed port
    /// contract** — including a C++ kernel's `InputSet<T>()` / `InputSetBuiltin(...)`. Sending
    /// one into a typed port fails loudly at the first packet rather than silently
    /// mismatching, but it fails at *runtime*, so prefer the right constructor up front:
    ///
    /// * a **built-in** payload → [`Packet::from_i64`], [`from_f64`](Packet::from_f64),
    ///   [`from_builtin`](Packet::from_builtin) and friends; these carry the proper `type_id`
    ///   and are what cross-language kernels exchange (see ADR #9);
    /// * a **custom** type that a C++ kernel must read by type → implement [`InteropType`] and use
    ///   [`Packet::from_interop`]. The low-level escape hatch is unsafe
    ///   [`Packet::new_interop`].
    ///
    /// `Packet::new` is still the right choice for a payload that never leaves Rust, or for
    /// ports that declare no type (the default).
    ///
    /// The `type_id` is deliberately **not** derived from Rust's [`std::any::TypeId`]: that is a
    /// different identity space from the C++ mangled-name hash, so auto-deriving it would
    /// produce ids that look cross-language but only agree with themselves — a silent
    /// mismatch, which is strictly worse than the current loud one.
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self {
            data: Some(Arc::new(PacketBody {
                payload: Arc::new(Payload::Native(Box::new(value))),
                metadata: Arc::new(Metadata::new()),
            })),
            ts: Timestamp::unset(),
        }
    }

    /// A value whose cross-language representation is declared by [`InteropType`].
    pub fn from_interop<T: InteropType>(value: T) -> Self {
        let type_id = fnv1a_type_id(T::TYPE_NAME);
        register_type_descriptor(
            type_id,
            T::TYPE_NAME,
            std::mem::size_of::<T>(),
            std::mem::align_of::<T>(),
        )
        .expect("conflicting InteropType declaration");
        // 安全性:InteropType 的 unsafe impl 承诺布局与稳定类型名的契约。
        unsafe { Self::new_interop(value, type_id) }
    }

    /// A Rust-native value carrying an explicit cross-language `type_id`.
    ///
    /// The id must match what the other side expects — typically
    /// [`fnv1a_type_id`] of the string a C++ kernel declared
    /// with `LMFLOW_DECLARE_TYPE_NAME`. The payload is held in the `Foreign` form, which gives
    /// the C side a readable pointer plus that id.
    ///
    /// Built-in ids (`0..=15`) are rejected: their layouts are owned by the engine and must be
    /// constructed with [`Packet::from_builtin`] or the corresponding `from_*` helper.
    ///
    /// # Safety
    ///
    /// `T` must have exactly the ABI layout associated with `type_id` in every foreign reader.
    /// In particular, do not assign an id for a different type merely to satisfy a port contract.
    pub unsafe fn new_interop<T: Any + Send + Sync>(value: T, type_id: u64) -> Self {
        assert!(
            type_id >= 16,
            "custom interop type_id must be >= 16; built-in ids 0..=15 are reserved"
        );
        let boxed = Box::new(value);
        let ptr = Box::into_raw(boxed) as *mut c_void;
        unsafe extern "C" fn drop_boxed<T>(p: *mut c_void) {
            // 安全性:p 来自同类型的 Box::into_raw,恰好回收一次。
            drop(unsafe { Box::from_raw(p as *mut T) });
        }
        Self {
            data: Some(Arc::new(PacketBody {
                payload: Arc::new(Payload::Foreign(Foreign {
                    ptr,
                    drop_fn: Some(drop_boxed::<T>),
                    type_id,
                    byte_size: type_descriptor(type_id)
                        .map_or(0, |descriptor| descriptor.size as u64),
                })),
                metadata: Arc::new(Metadata::new()),
            })),
            ts: Timestamp::unset(),
        }
    }

    /// 内建类型的便捷构造 —— 跨语言稳定,C++/Python 算子都能按类型读取。
    pub fn from_i64(v: i64) -> Self {
        Self::from_builtin(Builtin::I64(v))
    }
    pub fn from_f64(v: f64) -> Self {
        Self::from_builtin(Builtin::F64(v))
    }
    pub fn from_bool(v: bool) -> Self {
        Self::from_builtin(Builtin::Bool(v))
    }
    pub fn from_bytes(v: Vec<u8>) -> Self {
        Self::from_builtin(Builtin::Bytes(v))
    }
    /// 命名为 `from_string` 而非 `from_str`:后者会与标准库的
    /// `FromStr::from_str` 混淆(clippy 会拦)。
    pub fn from_string(v: &str) -> Self {
        Self::from_builtin(Builtin::Str(std::ffi::CString::new(v).unwrap_or_default()))
    }
    /// 读取内建整数;非整数包返回 None。
    pub fn as_i64(&self) -> Option<i64> {
        match self.as_builtin()? {
            Builtin::I64(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self.as_builtin()? {
            Builtin::F64(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self.as_builtin()? {
            Builtin::Str(s) => s.to_str().ok(),
            _ => None,
        }
    }

    pub fn from_builtin(b: Builtin) -> Self {
        Self {
            data: Some(Arc::new(PacketBody {
                payload: Arc::new(Payload::Builtin(b)),
                metadata: Arc::new(Metadata::new()),
            })),
            ts: Timestamp::unset(),
        }
    }

    /// 接管一个经过边界校验的外部 CPU buffer。
    ///
    /// # Safety
    /// 描述的全部地址在 Packet 生命周期内必须有效，且 release_fn 可跨线程调用一次。
    pub(crate) unsafe fn from_external_buffer(buffer: ExternalBuffer) -> Self {
        Self {
            data: Some(Arc::new(PacketBody {
                payload: Arc::new(Payload::Native(Box::new(buffer))),
                metadata: Arc::new(Metadata::new()),
            })),
            ts: Timestamp::unset(),
        }
    }

    /// 接管外部构造的 payload(C ABI 的 `owner==NULL` 形态)。
    ///
    /// # Safety
    /// `ptr` 必须有效,且 `drop_fn` 对它恰好可调用一次。
    pub unsafe fn from_foreign(
        ptr: *mut c_void,
        type_id: u64,
        drop_fn: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> Self {
        Self {
            data: Some(Arc::new(PacketBody {
                payload: Arc::new(Payload::Foreign(Foreign {
                    ptr,
                    drop_fn,
                    type_id,
                    byte_size: type_descriptor(type_id)
                        .map_or(0, |descriptor| descriptor.size as u64),
                })),
                metadata: Arc::new(Metadata::new()),
            })),
            ts: Timestamp::unset(),
        }
    }

    pub fn at(mut self, ts: Timestamp) -> Self {
        self.ts = ts;
        self
    }
    pub fn set_timestamp(&mut self, ts: Timestamp) {
        self.ts = ts;
    }
    pub fn timestamp(&self) -> Timestamp {
        self.ts
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_none()
    }
    pub fn type_id(&self) -> u64 {
        self.data.as_deref().map_or(type_id::NONE, |body| {
            Payload::type_id(body.payload.as_ref())
        })
    }
    pub fn byte_size(&self) -> u64 {
        self.data
            .as_deref()
            .map_or(0, |body| body.payload.byte_size())
    }
    pub fn payload(&self) -> Option<&Payload> {
        self.data.as_deref().map(|body| body.payload.as_ref())
    }

    /// 当前引用数(CoW 判定与测试用)。
    pub fn ref_count(&self) -> usize {
        self.data.as_ref().map_or(0, Arc::strong_count)
    }

    /// 底层 Arc 的借用 —— ffi 层构造「借用形态」的跨界包时用。
    pub(crate) fn arc_ref(&self) -> Option<&Arc<PacketBody>> {
        self.data.as_ref()
    }

    /// 交出底层 Arc(消耗自身)—— ffi 层构造「移交形态」时用。
    pub(crate) fn into_arc(self) -> Option<Arc<PacketBody>> {
        self.data
    }

    /// 由 Arc 与时间戳重建 —— ffi 层接管跨界包时用。
    pub(crate) fn from_arc(arc: Arc<PacketBody>, ts: Timestamp) -> Self {
        Self {
            data: Some(arc),
            ts,
        }
    }

    /// 取 Rust 原生值的引用;类型不符或非 Native 返回 None。
    pub fn get<T: Any>(&self) -> Option<&T> {
        match self.data.as_deref()?.payload.as_ref() {
            Payload::Native(b) => b.downcast_ref::<T>(),
            _ => None,
        }
    }

    /// 外部(C/C++)payload 的数据指针 —— 供 Rust 宿主读取 C++ 算子产出的包。
    ///
    /// 调用方须自行确认类型(比对 [`Packet::type_id`] 与 [`fnv1a_type_id`]),
    /// 并保证读取期间本 `Packet` 存活。
    pub fn foreign_ptr(&self) -> Option<*const c_void> {
        match self.data.as_deref()?.payload.as_ref() {
            Payload::Foreign(f) => Some(f.ptr as *const c_void),
            _ => None,
        }
    }

    pub fn as_builtin(&self) -> Option<&Builtin> {
        match self.data.as_deref()?.payload.as_ref() {
            Payload::Builtin(b) => Some(b),
            _ => None,
        }
    }

    pub fn metadata(&self) -> &Metadata {
        self.data
            .as_deref()
            .map_or(&EMPTY_METADATA, |body| body.metadata.as_ref())
    }

    pub fn metadata_value(&self, key: &str) -> Option<&MetadataValue> {
        self.metadata().get(key)
    }

    pub fn with_metadata<V: Into<MetadataValue>>(
        mut self,
        key: impl Into<String>,
        value: V,
    ) -> Self {
        self.set_metadata(key, value);
        self
    }

    pub fn set_metadata<V: Into<MetadataValue>>(&mut self, key: impl Into<String>, value: V) {
        let Some(body) = self.data.as_mut() else {
            return;
        };
        let body = Arc::make_mut(body);
        let metadata = Arc::make_mut(&mut body.metadata);
        metadata.insert(key.into(), value.into());
    }

    pub(crate) fn make_mutable_buffer(&mut self) -> Result<BufferView> {
        let body = Arc::make_mut(self.data.as_mut().ok_or_else(|| {
            Error::InvalidArg("cannot get a writable view of an empty packet".into())
        })?);
        let arc = &mut body.payload;
        let external = match arc.as_ref() {
            Payload::Native(value) => value.downcast_ref::<ExternalBuffer>(),
            _ => None,
        };
        let must_copy =
            Arc::strong_count(arc) > 1 || external.is_some_and(|buffer| buffer.readonly);
        if must_copy {
            let owned = match arc.as_ref() {
                Payload::Builtin(Builtin::Buffer(buffer)) => {
                    record_cow_copy(buffer.bytes.len() as u64);
                    buffer.clone()
                }
                Payload::Native(value) if value.is::<ExternalBuffer>() => {
                    let buffer = value
                        .downcast_ref::<ExternalBuffer>()
                        .expect("type checked above");
                    record_cow_copy(buffer.byte_size());
                    buffer.copy_to_owned()?
                }
                _ => {
                    return Err(Error::InvalidArg(
                        "this packet is not an LMFlowBuffer".into(),
                    ))
                }
            };
            *arc = Arc::new(Payload::Builtin(Builtin::Buffer(owned)));
        }

        match Arc::get_mut(arc).expect("buffer payload is exclusive after CoW") {
            Payload::Builtin(Builtin::Buffer(buffer)) => Ok(BufferView {
                data: buffer.bytes.as_mut_ptr() as *mut c_void,
                shape: buffer.shape,
                strides: buffer.strides,
                ndim: buffer.ndim,
                dtype: buffer.dtype,
                readonly: false,
            }),
            Payload::Native(value) if value.is::<ExternalBuffer>() => {
                let buffer = value
                    .downcast_mut::<ExternalBuffer>()
                    .expect("type checked above");
                Ok(BufferView {
                    data: buffer.data,
                    shape: buffer.shape,
                    strides: buffer.strides,
                    ndim: buffer.ndim,
                    dtype: buffer.dtype,
                    readonly: false,
                })
            }
            _ => Err(Error::InvalidArg(
                "this packet is not an LMFlowBuffer".into(),
            )),
        }
    }

    /// 写时复制:取得独占可写的内建 payload。
    ///
    /// 独占(引用数 1)→ 原地返回,**零拷贝**;被共享 → 复制一份后返回副本的可写引用。
    /// 仅支持 `Builtin` payload；adopt 的外部 Buffer 走专用 `make_mutable_buffer`。
    pub fn make_mutable_builtin(&mut self) -> Result<&mut Builtin> {
        let body = Arc::make_mut(self.data.as_mut().ok_or_else(|| {
            Error::InvalidArg("cannot get a writable view of an empty packet".into())
        })?);
        let arc = &mut body.payload;

        if Arc::get_mut(arc).is_none() {
            // 被共享:只有内建 payload 能复制
            let copy = match arc.as_ref() {
                Payload::Builtin(b) => {
                    record_cow_copy(b.byte_size());
                    Payload::Builtin(b.clone())
                }
                _ => {
                    return Err(Error::InvalidArg(
                        "custom payload cannot be copied by the engine (only builtin types support CoW)".into(),
                    ))
                }
            };
            *arc = Arc::new(copy);
        }
        // 此处必然独占
        match Arc::get_mut(arc).expect("just guaranteed exclusive") {
            Payload::Builtin(b) => Ok(b),
            _ => Err(Error::InvalidArg(
                "this packet is not a builtin payload; CoW not supported".into(),
            )),
        }
    }

    /// 供诊断日志使用的可读描述。
    pub fn debug_string(&self) -> String {
        let ty = match self.data.as_deref().map(|body| body.payload.as_ref()) {
            None => "Empty".to_string(),
            Some(Payload::Native(value)) => {
                if let Some(buffer) = value.downcast_ref::<ExternalBuffer>() {
                    format!(
                        "Buffer[{}] dtype={} external",
                        buffer.shape[..buffer.ndim as usize]
                            .iter()
                            .map(|dimension| dimension.to_string())
                            .collect::<Vec<_>>()
                            .join("x"),
                        buffer.dtype
                    )
                } else {
                    "Native".to_string()
                }
            }
            Some(Payload::Foreign(f)) => format!("Foreign(type#{})", f.type_id),
            Some(Payload::Builtin(b)) => match b {
                Builtin::Bytes(v) => format!("Bytes[{}]", v.len()),
                Builtin::I64(v) => format!("I64({v})"),
                Builtin::F64(v) => format!("F64({v})"),
                Builtin::Bool(v) => format!("Bool({v})"),
                Builtin::Str(s) => format!("Str({:?})", s.to_string_lossy()),
                Builtin::Buffer(b) => format!(
                    "Buffer[{}] dtype={}",
                    b.shape_slice()
                        .iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join("x"),
                    b.dtype
                ),
            },
        };
        format!("Packet{{type={ty}, ts={}}}", self.ts)
    }
}

impl std::fmt::Debug for Packet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.debug_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_roundtrip() {
        let p = Packet::new(42i32).at(Timestamp(7));
        assert_eq!(p.get::<i32>(), Some(&42));
        assert_eq!(
            p.get::<i64>(),
            None,
            "type mismatch must return None, not UB"
        );
        assert_eq!(p.timestamp(), Timestamp(7));
        assert!(!p.is_empty());
    }

    #[test]
    fn empty_packet() {
        let p = Packet::empty();
        assert!(p.is_empty());
        assert_eq!(p.type_id(), type_id::NONE);
        assert_eq!(p.timestamp(), Timestamp::unset());
    }

    #[test]
    fn clone_shares_and_does_not_copy() {
        let a = Packet::from_builtin(Builtin::I64(5));
        let b = a.clone();
        assert_eq!(a.ref_count(), 2);
        assert_eq!(b.ref_count(), 2);
        // 指向同一块 payload
        assert!(std::ptr::eq(
            a.payload().unwrap() as *const Payload,
            b.payload().unwrap() as *const Payload
        ));
    }

    #[test]
    fn cow_is_zero_copy_when_exclusive() {
        let mut p = Packet::from_builtin(Builtin::Buffer(
            BufferData::new(&[2, 3], dtype::U8).unwrap(),
        ));
        let before = p.payload().unwrap() as *const Payload;
        p.make_mutable_builtin().unwrap();
        let after = p.payload().unwrap() as *const Payload;
        assert!(
            std::ptr::eq(before, after),
            "no copy should happen when exclusive"
        );
    }

    #[test]
    fn cow_copies_when_shared_and_does_not_disturb_other() {
        let a = Packet::from_builtin(Builtin::Buffer(BufferData::new(&[4], dtype::U8).unwrap()));
        let mut b = a.clone();
        assert_eq!(a.ref_count(), 2);

        match b.make_mutable_builtin().unwrap() {
            Builtin::Buffer(buf) => buf.bytes[0] = 0xAB,
            _ => unreachable!(),
        }
        // b 改到了副本上;a 不受影响 —— 这正是「扇出后就地改写不污染其它分支」
        assert_eq!(a.ref_count(), 1, "a should be exclusive again after copy");
        match a.as_builtin().unwrap() {
            Builtin::Buffer(buf) => assert_eq!(buf.bytes[0], 0),
            _ => unreachable!(),
        }
        match b.as_builtin().unwrap() {
            Builtin::Buffer(buf) => assert_eq!(buf.bytes[0], 0xAB),
            _ => unreachable!(),
        }
    }

    #[test]
    fn cow_rejects_non_builtin() {
        let mut p = Packet::new(1u8);
        assert!(
            p.make_mutable_builtin().is_err(),
            "custom payload cannot be copied; must error rather than silently fail"
        );
    }

    #[test]
    fn buffer_strides_are_row_major_contiguous() {
        let b = BufferData::new(&[2, 3, 4], dtype::F32).unwrap();
        assert_eq!(b.ndim, 3);
        assert_eq!(b.shape_slice(), &[2, 3, 4]);
        // 最后一维步长 = 元素大小;向前依次乘
        assert_eq!(&b.strides[..3], &[3 * 4 * 4, 4 * 4, 4]);
        assert_eq!(b.bytes.len(), 2 * 3 * 4 * 4);
    }

    #[test]
    fn buffer_rejects_bad_args() {
        assert!(
            BufferData::new(&[], dtype::U8).is_err(),
            "ndim 0 is invalid"
        );
        assert!(
            BufferData::new(&[1, 2, 3, 4, 5, 6, 7, 8, 9], dtype::U8).is_err(),
            "must reject exceeding MAX_DIMS"
        );
        assert!(
            BufferData::new(&[2, 2], 999).is_err(),
            "must reject unknown dtype"
        );
        assert!(
            BufferData::new(&[-1], dtype::U8).is_err(),
            "must reject negative shape"
        );
    }

    #[test]
    fn dtype_sizes() {
        assert_eq!(dtype_size(dtype::U8), 1);
        assert_eq!(dtype_size(dtype::F16), 2);
        assert_eq!(dtype_size(dtype::F32), 4);
        assert_eq!(dtype_size(dtype::F64), 8);
        assert_eq!(dtype_size(12345), 0, "unknown dtype returns 0");
    }

    #[test]
    fn foreign_drop_fn_called_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn drop_it(p: *mut c_void) {
            CALLS.fetch_add(1, Ordering::SeqCst);
            drop(unsafe { Box::from_raw(p as *mut u32) });
        }
        let raw = Box::into_raw(Box::new(7u32)) as *mut c_void;
        {
            let p = unsafe { Packet::from_foreign(raw, 99, Some(drop_it)) };
            let q = p.clone();
            assert_eq!(q.type_id(), 99);
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                0,
                "must not free while references remain"
            );
        }
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "must free exactly once when refcount hits zero"
        );
    }

    #[test]
    fn fnv1a_matches_cpp_sugar_layer() {
        // 与 include/flow.hpp 的 Fnv1a + NormalizeTypeId 必须一致。
        // 这两个期望值取自实际编译 flow.hpp 后的输出(见提交说明)。
        assert_eq!(fnv1a_type_id("i"), 12638195996648667684);
        assert_eq!(fnv1a_type_id("d"), 12638183902020757363);
        assert!(
            fnv1a_type_id("") >= 16,
            "empty name must also avoid the builtin range"
        );
    }

    #[test]
    fn interop_packet_carries_explicit_type_id() {
        let tid = fnv1a_type_id("i");
        let p = unsafe { Packet::new_interop(7i32, tid) };
        assert_eq!(p.type_id(), tid);
        // 以 Foreign 形态承载,故指针可读、C 布局兼容
        let ptr = p.foreign_ptr().expect("should get a data pointer");
        assert_eq!(unsafe { *(ptr as *const i32) }, 7);
    }

    #[test]
    #[should_panic(expected = "built-in ids 0..=15 are reserved")]
    fn interop_packet_rejects_builtin_type_ids() {
        let _ = unsafe { Packet::new_interop(7i32, type_id::I64) };
    }

    #[test]
    fn declared_interop_type_uses_stable_name() {
        #[repr(C)]
        struct Point {
            x: i32,
            y: i32,
        }
        unsafe impl InteropType for Point {
            const TYPE_NAME: &'static str = "lmflow.test.Point";
        }

        let p = Packet::from_interop(Point { x: 3, y: 4 });
        assert_eq!(p.type_id(), fnv1a_type_id(Point::TYPE_NAME));
        assert_eq!(type_name(p.type_id()), Point::TYPE_NAME);
        assert_eq!(p.byte_size(), std::mem::size_of::<Point>() as u64);
        let point = unsafe { &*(p.foreign_ptr().unwrap() as *const Point) };
        assert_eq!((point.x, point.y), (3, 4));
    }

    #[test]
    fn strict_type_registration_rejects_identity_and_layout_conflicts() {
        let id = fnv1a_type_id("lmflow.test.StrictRegistration");
        register_type_descriptor(id, "lmflow.test.StrictRegistration", 8, 4).unwrap();
        register_type_descriptor(id, "lmflow.test.StrictRegistration", 8, 4).unwrap();

        let layout_error =
            register_type_descriptor(id, "lmflow.test.StrictRegistration", 16, 8).unwrap_err();
        assert!(layout_error.to_string().contains("already registered"));

        let name_error =
            register_type_descriptor(id, "lmflow.test.OtherRegistration", 8, 4).unwrap_err();
        assert!(name_error.to_string().contains("stable-name id"));
    }

    #[test]
    fn strict_type_registration_rejects_noncanonical_id() {
        let name = "lmflow.test.NonCanonical";
        let expected = fnv1a_type_id(name);
        let error = register_type_descriptor(expected + 1, name, 8, 8).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(name), "{message}");
        assert!(message.contains(&expected.to_string()), "{message}");
    }

    #[test]
    fn registered_foreign_payload_uses_descriptor_size() {
        unsafe extern "C" fn drop_u64(p: *mut c_void) {
            drop(unsafe { Box::from_raw(p as *mut u64) });
        }
        let name = "lmflow.test.ForeignSize";
        let id = fnv1a_type_id(name);
        register_type_descriptor(id, name, 8, 8).unwrap();
        let ptr = Box::into_raw(Box::new(42u64)) as *mut c_void;
        let packet = unsafe { Packet::from_foreign(ptr, id, Some(drop_u64)) };
        assert_eq!(packet.byte_size(), 8);
    }

    #[test]
    fn byte_size_accounting() {
        assert_eq!(Packet::from_builtin(Builtin::I64(0)).byte_size(), 8);
        assert_eq!(
            Packet::from_builtin(Builtin::Bytes(vec![0; 100])).byte_size(),
            100
        );
        // 未声明布局的 Rust-native payload 仍无法计量。
        assert_eq!(Packet::new(0u64).byte_size(), 0);
    }

    #[test]
    fn debug_string_is_informative() {
        let p = Packet::from_builtin(Builtin::Buffer(
            BufferData::new(&[3, 224, 224], dtype::F32).unwrap(),
        ))
        .at(Timestamp(42));
        let s = p.debug_string();
        assert!(s.contains("3x224x224"), "should contain shape: {s}");
        assert!(s.contains("42"), "should contain timestamp: {s}");
    }
}
