# lmflow-core

Engine crate for **lmflow**, a dataflow-graph compute framework: a Rust scheduler behind a
stable C ABI, with C++ kernels compiled in at build time.

This is the low-level engine (library name `flow_core` → `libflow_core.a`, shared by the C ABI,
CMake, and Python bindings). Most Rust users want the [`lmflow`](https://crates.io/crates/lmflow)
facade crate instead, which re-exports this API under a nicer name:

```rust
use lmflow::{Graph, Packet, Timestamp};
```

Building compiles the bundled C++ kernels via the [`cc`](https://crates.io/crates/cc) crate, so a
C++ toolchain (g++/clang) must be available.

License: Apache-2.0.
