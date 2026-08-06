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
//! `cargo add lmflow` needs no C++ toolchain. Kernels are always supplied explicitly by the host
//! or by a linked kernel component; the engine itself only schedules them and never interprets
//! payloads.
//!
//! # Running a graph
//!
//! ```
//! use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet, Timestamp};
//!
//! #[derive(Default)]
//! struct Relay;
//!
//! impl Kernel for Relay {
//!     fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
//!         context.forward(0, 0)
//!     }
//! }
//!
//! # fn main() -> lmflow::Result<()> {
//! register_kernel::<Relay>("Relay")?;
//! let graph = Graph::from_yaml(
//!     r#"
//! nodes:
//!   - { name: relay, kernel: Relay, input_ports: [in], output_ports: [out] }
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
//! The lm-flow repository also carries 18 optional C++ kernels (`../cpp/kernels/`). They are built
//! separately by CMake as `lmflow::kernels`; this crate never compiles or bundles C++.
//!
//! Full documentation, including the C/C++ and Python interfaces:
//! <https://laomou.github.io/lm-flow/>. The authoritative design document — scheduling model,
//! timestamp and termination semantics, lock ordering rules and the decision log — is
//! `docs/design.md` (written in Chinese).

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
    DotView, Graph, Input, Poller, PollerBackpressureStatsSnapshot, PollerOptions, PollerOverflow,
    State, WatermarkBackpressureStatsSnapshot,
};
pub use kernel_api::{register_kernel, Kernel, KernelContract, KernelCtx};
pub use packet::{BufferData, Builtin, InteropType, Packet};
pub use status::{Error, Result};
pub use timestamp::Timestamp;
