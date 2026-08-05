# lmflow

A **dataflow-graph compute engine**. Computation is described as a directed graph: nodes are
**kernels**, and **timestamped packets** flow along the edges. The engine — scheduling, threads,
edge queues, topology, YAML parsing — is written in Rust and exposed through a single stable
**C ABI**, so hosts and kernels can live in whatever language suits them.

```text
  Host (Rust / C++ / Python) ── drives the graph
        │  C ABI  (lmflow/flow.h)
        ▼
  Engine (Rust): scheduler · executors · edge queues · topology · YAML
        │  C ABI  (callbacks)
        ▼
  Kernels: Rust (trait Kernel)  ·  C++ (flow.hpp sugar)  ·  Python
```

## API reference

<div class="cards">
  <a href="rust/"><strong>Rust API</strong><span>The engine crate: <code>Graph</code>, <code>Packet</code>, <code>trait Kernel</code>, <code>register_kernel</code>.</span></a>
  <a href="cpp/"><strong>C / C++ guide</strong><span>The stable C ABI, the <code>flow.hpp</code> kernel sugar layer, and embedding.</span></a>
  <a href="python/"><strong>Python API</strong><span>The <code>lmflow</code> package: graphs, kernels and pollers from Python.</span></a>
</div>

The Rust docs above track `main`. For the version you get from crates.io, see
[docs.rs/lmflow](https://docs.rs/lmflow). The authoritative design document —
scheduling model, timestamp semantics, lock ordering and the ADR log — is
[here](design/) (written in Chinese).

## Core concepts

| Concept | Description |
|---|---|
| `Graph` | A computation graph described in YAML; handles init, start, input-close and termination |
| `Node` | A node in the graph, holding one kernel instance |
| `Kernel` | User-written compute logic implementing `Open` / `Process` / `Close` |
| `Edge` / `Port` | Edges and ports connect producers to consumers by name; an edge carries a queue of timestamped packets |
| `Packet` | An immutable shared payload plus a timestamp; cloning bumps a refcount rather than copying |
| `Contract` | The port type contract a kernel declares in `GetContract`, validated at graph-build time |
| `Poller` / `Observer` | Two ways to take graph output: pull (blocking / timeout / non-blocking) and push (callback) |

A kernel never knows what language its neighbours are written in. Rust, C++ and Python kernels all
register into the same registry through the same function-pointer vtable, and the engine treats
them identically — including in the same graph.

## Quick start

### Rust

The published crate is the **pure-Rust engine**: no C++ toolchain is required.

```bash
cargo add lmflow
```

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

register_kernel::<Double>("Double")?;   // your own kernel
let graph = Graph::from_yaml(yaml)?;    // `PassThrough` / `Sink` need no registration
```

Two structural kernels ship with the engine and are registered automatically: `PassThrough`
(zero-copy forward) and `Sink` (consume only, so a branch can terminate itself). Anything that
would have to assume a concrete payload type is deliberately left to you — the engine never
interprets payloads.

### Python

```bash
pip install lm-lmflow          # prebuilt wheels; import name is `lmflow`
```

```python
import lmflow

@lmflow.kernel("Double")
class Double(lmflow.Kernel):
    def process(self, cc):
        cc.emit(0, cc.input(0).as_int() * 2)

g = lmflow.Graph.from_yaml("""
nodes:
  - { name: d, kernel: Double, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
""")
out = g.add_poller("out")
g.start()
g.input("in").send(21, ts=0)
print(out.next(timeout=5.0).as_int())      # 42
g.close_all_inputs()
g.wait_done(timeout=5.0)
```

### C / C++

C, C++ and mobile hosts use the headers plus the static library directly. Each tagged release
ships a per-platform SDK tarball (Linux x86_64/aarch64, macOS arm64, iOS arm64, Android arm64):

```cmake
find_package(lmflow REQUIRED)
target_link_libraries(my_app PRIVATE lmflow::core)
```

```cpp
class DoubleKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) { c.InputSetAny(0); c.OutputSetAny(0); }
  lmflow::Status Process(lmflow::Context& cc) override {
    cc.Forward(0, 0);                       // zero-copy passthrough
    return lmflow::Status::Ok();
  }
};
```

See the [C / C++ guide](cpp/) for the ABI contract, kernel authoring, packet types, runtime
control and mobile embedding.

## Graphs are declarative

The topology, the threading and the flow-control policy all live in YAML, not in your kernels:

```yaml
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 4 }

nodes:
  - name: camera
    kernel: MySource
    executor: cpu
    output_ports: [frames]
    rate: 30                     # engine-paced source: 30 Hz, no sleep in the kernel

  - name: detect
    kernel: MyDetector
    executor: cpu
    input_ports: [frames]
    output_ports: [boxes]
    on_error: skip               # drop the bad frame, keep the pipeline alive
    input_policy: { type: immediate }

input_ports: []
output_ports: [boxes]
max_queue_size: 100
watchdog_ms: 5000
```

Notable capabilities:

- **Input policies** per node — `sync` (timestamp-aligned), `immediate`, `fixed_size`,
  `sync_set` (grouped alignment) and `batch`.
- **Node-level error policy** — `on_error: abort` (default) or `skip`, which discards the
  offending packet, advances the downstream timestamp bound, counts it and logs a warning.
  A long-running pipeline should not die because one frame was bad.
- **Declarative source pacing** — `rate: N` (Hz) makes the engine guarantee a minimum interval
  between `process` calls on a source node, so the kernel needs no timing code of its own.
- **Feedback loops** — `back_edges` turn a named input port into a latest-value register that
  does not participate in readiness, termination or timestamp alignment, which is what lets a
  topology contain a cycle.
- **Reusable subgraphs** — define once under `subgraphs:`, instantiate with `type:`, inlined at
  graph-build time; `include:` pulls definitions in from other files.
- **Reset and re-run** — after termination a graph can be reset and started again while keeping
  already-opened kernel instances, so an expensive model load is not repeated per session.
- **Observability** — per-node statistics (packets in/out, invocation and error counts, peak queue
  depth, processing-time totals), a watchdog for slow callbacks, and DOT export with an optional
  latency heat map.
- **Executor control** — thread count, CPU affinity and real-time priority per executor, for
  pinning work on realtime or NUMA systems. Nodes that name no executor land on the engine-owned
  **default executor**, a thread pool sized to the CPU count. Declare your own
  `DelegatingExecutor` and point nodes at it for serialized host-thread execution instead
  (deterministic order; Python kernels on the same graph do not contend with each other for the GIL).

Unsupported configuration is **rejected loudly** rather than ignored: if a field asks for
something this version does not implement, graph construction fails. Silently doing less than
what the config asked for is the one failure mode that cannot be debugged from the outside.

## Where next

- [Rust API reference](rust/) · [C / C++ guide](cpp/) · [Python API reference](python/)
- [Design document](design/) — the authoritative description of the scheduling model, timestamp
  and termination semantics, lock ordering rules, and the architectural decision log (Chinese)
- [Source on GitHub](https://github.com/laomou/lm-flow) — including runnable examples for Rust,
  C++, Python, Android (JNI), iOS (Swift) and HarmonyOS (NAPI)
