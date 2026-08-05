# lmflow

[![crates.io](https://img.shields.io/crates/v/lmflow.svg?logo=rust)](https://crates.io/crates/lmflow)
[![docs.rs](https://docs.rs/lmflow/badge.svg)](https://docs.rs/lmflow)
[![ci](https://github.com/laomou/lm-flow/actions/workflows/ci.yml/badge.svg)](https://github.com/laomou/lm-flow/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/laomou/lm-flow/blob/main/LICENSE)

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

Full documentation: <https://docs.rs/lmflow> (this crate) ·
<https://laomou.github.io/lm-flow/> (the whole project: Rust / C++ / Python).

### Built-in Rust kernels

Two, registered automatically on graph construction — no setup call needed:

| Name | What it does | Ports |
|---|---|---|
| `PassThrough` | zero-copy forward (wiring / placeholder) | 1 → 1, any type |
| `Sink` | consume only, so a branch can terminate itself | 1 → 0, any type |

Deliberately only these two: both are **purely structural, with no assumptions about the payload**.
Kernels like `Scale`/`Sum`/`Zip`/`Filter` would have to assume a concrete payload type (i64), which
contradicts the engine's design rule that it never interprets payloads. Fan-out needs no kernel
either — one edge can feed several consumers natively. Compute is yours to write; the engine
provides structure.

### Declared in YAML, not in your kernels

Topology, threading and flow-control policy are configuration, not code:

| Field | What it does |
|---|---|
| `executors` | thread pools, with optional CPU `affinity` and real-time `priority` |
| `input_policy` | per node: `sync` (timestamp-aligned), `immediate`, `fixed_size` (bounded, drops oldest), `sync_set`, `batch` |
| `on_error` | `abort` (default) or `skip` — drop the offending packet, advance the downstream bound, count it and warn, instead of killing the pipeline |
| `rate` | source pacing in Hz; the engine guarantees the interval, so the kernel needs no timing code |
| `back_edges` | turn an input port into a latest-value register that is excluded from readiness, termination and alignment — this is what lets a topology contain a cycle |
| `subgraphs` / `type` / `include` | reusable subgraphs, inlined at graph-build time |
| `watchdog_ms`, `stats`, `max_queued_*` | slow-callback warnings, tiered runtime diagnostics, global watermarks |

`Graph::reset()` puts a terminated graph back to a startable state while keeping already-opened
kernel instances alive, so an expensive one-off such as loading a model is not repeated per run.
Per-node statistics (packets in/out, invocations, errors, peak queue depth, processing time) and
`Graph::to_dot()` — with an optional latency heat map — cover observability.

Configuration that asks for something unimplemented is **rejected loudly** at graph construction
rather than ignored: silently doing less than the config asked for is the one failure mode that
cannot be debugged from the outside.

The same engine is also consumed from C/C++ (`#include "lmflow/flow.h"`, link `liblmflow.a`),
Python (`pip install lm-lmflow`), and mobile (Android / iOS / HarmonyOS bridges). See the
[repository](https://github.com/laomou/lm-flow) for the native SDK and the 18 bundled C++ kernels —
those live outside this crate and are not distributed with it.

License: Apache-2.0.
