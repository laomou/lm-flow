//! **lmflow** —— 数据流图引擎的 Rust 门面。
//!
//! 引擎实体在 [`flow_core`] crate;本 crate 只是薄薄地 re-export 它的公开 API,让外部 Rust
//! 用 `use lmflow::{Graph, Packet, Timestamp, Kernel, register_kernel}`。
//!
//! ```ignore
//! use lmflow::{Graph, Packet, Timestamp};
//! lmflow::register_builtin_kernels();
//! let g = Graph::from_yaml(yaml)?;
//! ```
//!
//! 写 Rust 算子:实现 [`Kernel`] + [`register_kernel`];端口类型 id 见 [`packet::type_id`]。

pub use flow_core::{
    register_builtin_kernels, register_kernel, BufferData, Builtin, Error, Graph, Input, Kernel,
    KernelContract, KernelCtx, Packet, Poller, Result, State, Timestamp,
};

/// 包类型 / dtype 常量(`packet::type_id::I64` 等),写算子契约时用。
pub use flow_core::packet;
