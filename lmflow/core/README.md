# lmflow

A dataflow-graph compute engine: a Rust scheduler behind a stable C ABI, with C++ kernels compiled
in at build time.

```rust
use lmflow::{Graph, Packet, Timestamp};

lmflow::register_builtin_kernels();
let g = Graph::from_yaml(yaml)?;
```

## Install

```sh
cargo add lmflow
```

Building compiles the bundled C++ kernels via the [`cc`](https://crates.io/crates/cc) crate, so a
C++ toolchain (g++/clang) must be on `PATH`. The same engine is also consumed from C/C++ (stable C
ABI, `#include "lmflow/flow.h"`), Python, and mobile — this crate is the Rust entry point.

License: Apache-2.0.
