# lmflow

A dataflow-graph compute engine: a **pure-Rust** scheduler behind a stable C ABI. Computation is
described as a directed graph — nodes are **kernels**, and **timestamped packets** flow along the
edges.

```rust
use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet};

#[derive(Default)]
struct Double;
impl Kernel for Double {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let v = cc.input(0).and_then(|p| p.as_i64()).unwrap_or(0);
        cc.emit(0, Packet::from_i64(v * 2))
    }
}

register_kernel::<Double>("Double")?;
let g = Graph::from_yaml(yaml)?;
```

## Install

```sh
cargo add lmflow
```

**No C++ toolchain required.** Kernels are decoupled from the engine: write them in Rust with
`trait Kernel` + `register_kernel`, or plug C++/Python kernels in through the C ABI
(`lmflow_register_kernel`) — the registry is language-agnostic, and the engine neither knows nor
cares what language a kernel is written in.

The same engine is also consumed from C/C++ (`#include "lmflow/flow.h"`, link `liblmflow.a`),
Python (`pip install lm-lmflow`), and mobile (Android / iOS / HarmonyOS bridges). See the
[repository](https://github.com/laomou/lm-flow) for the native SDK and the 18 bundled C++ kernels —
those live outside this crate and are not distributed with it.

License: Apache-2.0.
