//! A dataflow-graph compute engine.
//!
//! Computation is described as a directed graph: nodes are **kernels**, and **timestamped
//! packets** flow along the edges. This crate is the engine itself — scheduling, worker threads,
//! edge queues, topology validation and YAML parsing.
//!
//! # Two interfaces
//!
//! * **Rust API** — the public items of this crate ([`Graph`], [`Packet`], [`Kernel`], …).
//! * **C ABI** — the [`ffi`] module, declared in `include/lmflow/flow.h`. This is the only
//!   *stable* interface, and it is what C, C++, Python and mobile hosts use.
//!
//! # Kernels are decoupled from the engine
//!
//! A kernel can be written in **Rust** ([`Kernel`] + [`register_kernel`]), in **C++** (the
//! header-only `flow.hpp` sugar layer), or in **Python** (pybind11). All three register into one
//! registry through the same function-pointer vtable, so the engine neither knows nor cares which
//! language a kernel came from — and kernels written in different languages can be mixed freely
//! in a single graph.
//!
//! This crate is a **pure-Rust engine by default**: it neither compiles nor bundles any C++, so
//! `cargo add lmflow` needs no C++ toolchain. Two structural kernels ship with it and are
//! registered automatically when a graph is built (see [`builtin`]): `PassThrough` (zero-copy
//! forward) and `Sink` (consume only, so a branch can terminate itself). Anything that would have
//! to assume a concrete payload type is deliberately left to you — the engine never interprets
//! payloads.
//!
//! # Running a graph
//!
//! ```
//! use lmflow::{Graph, Packet, Timestamp};
//!
//! # fn main() -> lmflow::Result<()> {
//! let graph = Graph::from_yaml(
//!     r#"
//! nodes:
//!   - { name: relay, kernel: PassThrough, input_ports: [in], output_ports: [out] }
//! input_ports: [in]
//! output_ports: [out]
//! "#,
//! )?;
//!
//! let out = graph.add_poller("out")?;
//! graph.start()?;
//! graph.input("in")?.send(Packet::from_i64(21).at(Timestamp(0)))?;
//!
//! assert_eq!(out.next().and_then(|p| p.as_i64()), Some(21));
//!
//! graph.close_all_inputs();
//! graph.wait_done()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Writing a kernel
//!
//! ```
//! use lmflow::{register_kernel, Kernel, KernelCtx, Packet};
//!
//! #[derive(Default)]
//! struct Double;
//!
//! impl Kernel for Double {
//!     fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
//!         let v = cc.input(0).and_then(|p| p.as_i64()).unwrap_or(0);
//!         cc.emit(0, Packet::from_i64(v * 2))
//!     }
//! }
//!
//! # fn main() -> lmflow::Result<()> {
//! register_kernel::<Double>("Double")?; // once, before Graph::from_yaml
//! # Ok(())
//! # }
//! ```
//!
//! # Beyond this crate
//!
//! The lm-flow repository also carries 18 built-in C++ kernels (`../cpp/kernels/`), compiled in by
//! the `builtin-kernels` feature. That feature is **off by default and only usable inside the
//! repository**: those sources live outside the crate directory, so they are not distributed with
//! the published crate.
//!
//! Full documentation, including the C/C++ and Python interfaces:
//! <https://laomou.github.io/lm-flow/>. The authoritative design document — scheduling model,
//! timestamp and termination semantics, lock ordering rules and the decision log — is
//! `docs/design.md` (written in Chinese).

pub mod builtin;
pub mod config;
pub mod context;
pub mod executor;
mod expand;
pub mod ffi;
pub mod graph;
pub mod kernel;
pub mod kernel_api;
pub mod packet;
pub mod runtime;
pub mod status;
pub mod timestamp;

pub use graph::{
    Graph, Input, Poller, PollerBackpressureStatsSnapshot, PollerOptions, PollerOverflow, State,
    WatermarkBackpressureStatsSnapshot,
};
pub use kernel_api::{register_kernel, Kernel, KernelContract, KernelCtx};
pub use packet::{BufferData, Builtin, InteropType, Packet};
pub use status::{Error, Result};
pub use timestamp::Timestamp;

#[cfg(feature = "builtin-kernels")]
extern "C" {
    /// 由 `../cpp/kernels/register.cc` 提供:显式聚合注册内置 C++ 算子的实现。
    ///
    /// 用显式函数而非静态初始化,是因为静态初始化对象在静态库中可能被链接器裁剪
    /// (见 docs/design.md §14 风险登记)。C ABI 的 `lmflow_register_builtin_kernels`
    /// 由下方 Rust 包装导出(这样它也能出现在 cdylib 的动态导出表里)。
    fn lmflow_register_builtin_kernels_impl();
}

/// Register the built-in C++ kernels. **Idempotent** — only the first call takes effect.
///
/// Must be called before [`Graph::from_yaml`], otherwise graph construction reports an
/// unregistered kernel.
///
/// Only exists under the `builtin-kernels` feature (**off by default**, and only usable inside the
/// lm-flow repository); otherwise register your own Rust kernels with [`register_kernel`].
#[cfg(feature = "builtin-kernels")]
pub fn register_builtin_kernels() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe { lmflow_register_builtin_kernels_impl() });
}

/// C ABI: register the built-in kernels (see `include/lmflow/flow.h`). This is the exported
/// wrapper around [`register_builtin_kernels`] — defined in Rust and `#[no_mangle]`-exported so it
/// appears in the symbol table of both the static library and the cdylib, matching the declaration
/// in `flow.h`.
///
/// # Safety
/// Takes no arguments and no pointers, and is internally idempotent. Safe to call from any thread.
#[cfg(feature = "builtin-kernels")]
#[no_mangle]
pub unsafe extern "C" fn lmflow_register_builtin_kernels() {
    register_builtin_kernels();
}
