# lmflow

Rust facade for the **lmflow** dataflow-graph engine. Thinly re-exports the
[`lmflow-core`](https://crates.io/crates/lmflow-core) engine so you can write `use lmflow::…`
instead of `use flow_core::…`.

```rust
use lmflow::{Graph, Packet, Timestamp};

lmflow::register_builtin_kernels();
let g = Graph::from_yaml(yaml)?;
```

## Install

```sh
cargo add lmflow
```

Building pulls in `lmflow-core`, whose build script compiles the engine's C++ kernels via the
[`cc`](https://crates.io/crates/cc) crate — a C++ toolchain (g++/clang) must be on `PATH`.

License: Apache-2.0.
