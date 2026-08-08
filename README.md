# lm-flow

**English** | [简体中文](README.zh-CN.md)

[![crates.io](https://img.shields.io/crates/v/lmflow.svg?logo=rust)](https://crates.io/crates/lmflow)
[![PyPI](https://img.shields.io/pypi/v/lm-lmflow.svg?logo=pypi&logoColor=white)](https://pypi.org/project/lm-lmflow/)
[![docs](https://img.shields.io/badge/docs-lm--flow-blue)](https://laomou.github.io/lm-flow/)
[![ci](https://github.com/laomou/lm-flow/actions/workflows/ci.yml/badge.svg)](https://github.com/laomou/lm-flow/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A dataflow-graph engine: computation is described as a **directed graph** — nodes are **kernels**, and **timestamped packets** flow along the edges. The engine is written in Rust (scheduling, threads, queues, topology) and exposes a single stable **C ABI**; kernels can be written in **Rust**, **C++** or **Python**.

📖 **Documentation**: <https://laomou.github.io/lm-flow/> — [Rust](https://laomou.github.io/lm-flow/rust/) · [C / C++](https://laomou.github.io/lm-flow/cpp/) · [Python](https://laomou.github.io/lm-flow/python/)

```text
  Host (Rust / C++ / Python) ── drives the graph
        │  C ABI  (lmflow/flow.h)
        ▼
  Engine (Rust): scheduler · executors · edge queues · topology · YAML
        │  C ABI  (callbacks)
        ▼
  Kernels: Rust (trait Kernel)  /  C++ (flow.hpp sugar)  /  Python
```

## Layout

First-party source lives under `lmflow/`; build files and the vendored submodule sit at the repo root (which is also the Python project root, so wheels bundle everything they need).

```text
lm-flow/
├── lmflow/                    All first-party source
│   ├── core/                  Engine — the `lmflow` crate, **pure Rust** (package = lib = lmflow → liblmflow.a)
│   │   ├── build.rs           Repository-only Rust test path for bundled C++ kernels
│   │   ├── Cargo.toml · Cargo.lock
│   │   └── src/ · tests/ · benches/
│   ├── include/lmflow/        Public core headers — flow.h · flow.hpp · flow_platform_log.hpp
│   ├── adapters/opencv/       Optional OpenCV adapter — <lmflow/opencv.hpp> · lmflow::opencv
│   ├── cpp/                   C++ side (not the engine) — kernels/ (18 built-ins) · abi_assert.cc · tests/
│   ├── python/                pybind11 bindings (src/) + the lmflow package + CMakeLists
│   └── examples/              each example is self-contained: examples/<lang>/<name>/
│       ├── cpp/               hello_world/, custom_type/  (find_package or build-from-source)
│       ├── rust/              hello_world/  (a standalone cargo project)
│       ├── python/            hello_world/, async_pipeline/, realtime_pipeline/, opencv_pipeline/
│       └── {android,ios,harmonyos}/hello_world/   mobile integration examples
├── third_party/pybind11/      vendored git submodule (only used to build the Python wheel)
├── cmake/                     engine.cmake · install-sdk.cmake · find_package config
├── docs/                      design.md (authoritative design doc, Chinese) · web/ (doc-site sources)
├── CHANGELOG.md               Release notes (crate · wheel · native SDK share one version)
├── CMakeLists.txt             Top-level build (drives cargo; C/C++ SDK + Python extension)
└── pyproject.toml             Python wheel (scikit-build-core → the same CMake)
```

## Core concepts

| Concept | Description |
|---|---|
| `Graph` | A computation graph described in YAML; handles init, start, input-close and termination |
| `Node` | A node in the graph, holding one kernel instance |
| `Kernel` | User-written compute logic implementing `Open/Process/Close` |
| `Edge/Port` | Edges and ports connect producers to consumers by name; an edge carries a queue of timestamped packets |
| `Packet` | A packet = an immutable shared payload + a timestamp |
| `Contract` | Port type contract, declared by the kernel in `GetContract` |
| `Poller/Observer` | Pull or push graph output; bounded Pollers support block/drop/latest policies |

Concrete port contracts are checked across edges while the graph is built, so a known
`I64 → F64` connection fails before execution. An `any` endpoint keeps runtime packet checks,
and a kernel's emitted packets must also satisfy its own output contract.

## Quick start

Rust — from crates.io, **no C++ toolchain needed**:

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

register_kernel::<Double>("Double")?;          // register before building the graph
let g = Graph::from_yaml(yaml)?;
```

The published crate is the **pure-Rust engine**. The 18 bundled C++ kernels are a separate CMake
component and are never compiled by the core crate.

Working in this repo instead (the engine crate is `lmflow/core`):

```bash
cd lmflow/core
cargo build     # pure-Rust engine, no C++ compiled
cargo test      # unit tests + ABI layout + Rust-kernel tests

# C++ kernels are built and tested through the root CMake project:
cd ../..
cmake -B build -DLMFLOW_BUILD_KERNELS=ON
cmake --build build
ctest --test-dir build
```

Python:

```bash
pip install lm-lmflow               # prebuilt wheels (Linux manylinux / macOS)
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
print(out.next(timeout=5.0).as_int())   # 42
g.close_all_inputs(); g.wait_done(timeout=5.0)
```

> Only prebuilt wheels are published — if no wheel matches your platform, `pip install` fails rather than compiling on your machine. To build locally instead, clone with submodules and build the wheel from source (needs a Rust toolchain + CMake):
>
> ```bash
> git clone --recursive https://github.com/laomou/lm-flow
> pip install ./lm-flow          # scikit-build-core drives CMake → cargo + pybind11
> ```

A kernel looks like this (C++, using the sugar layer):

```cpp
class PassThroughKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) { c.InputSetAny(0); c.OutputSetAny(0); }
  lmflow::Status Process(lmflow::Context& cc) override {
    cc.Forward(0, 0);                      // zero-copy passthrough
    return lmflow::Status::Ok();
  }
};
```

A graph looks like this (YAML):

```yaml
nodes:
  - name: "scale"
    kernel: "ScaleKernel"
    input_ports: ["in"]
    output_ports: ["out"]
    options: { factor: 3 }
input_ports: ["in"]
output_ports: ["out"]
```

See [`docs/design.md`](docs/design.md) for the full design.

### Validate a graph before deployment

The core crate includes a configuration-only checker. It parses YAML, resolves
`include`, expands subgraphs, and validates ports, topology, queue policies, and
executor references without loading kernels or starting worker threads:

```bash
cargo run --manifest-path lmflow/core/Cargo.toml --bin lmflow -- \
  check-config graph.yaml
cargo run --manifest-path lmflow/core/Cargo.toml --bin lmflow -- \
  check-config graph.yaml --json
```

The JSON output is suitable for CI tooling. An editor schema is available at
[`docs/lmflow-config.schema.json`](docs/lmflow-config.schema.json).

## Native SDK (C / C++ / mobile)

C/C++ and mobile hosts don't use pip — they use the **headers + library** directly. Each tagged GitHub Release ships per-platform `lmflow-<version>-<platform>.tar.gz` (Linux x86_64/aarch64, macOS arm64, iOS arm64, Android arm64):

```text
lmflow-v0.3.0-linux-x86_64/
├── include/lmflow/   flow.h · flow.hpp · flow_platform_log.hpp
└── lib/       liblmflow_core.a · liblmflow_kernels.a · liblmflow.so
```

```cmake
# Recommended: the target preserves all static kernel registrars cross-platform.
find_package(lmflow REQUIRED)
target_link_libraries(my_host PRIVATE lmflow::lmflow)
```

Without CMake, retain the complete kernels archive explicitly:

```bash
g++ -std=c++17 -Iinclude my_host.cc \
  -Wl,--whole-archive lib/liblmflow_kernels.a -Wl,--no-whole-archive \
  lib/liblmflow_core.a -lpthread -ldl -lm -o my_host
```

Or build the native SDK locally:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release \
  -DLMFLOW_BUILD_SHARED_LIBS=ON -DLMFLOW_BUILD_KERNELS=ON
cmake --build build
```

### Build & consume with CMake

CMake is the top-level build for the C++/native side. Cargo builds the pure Rust core, while
CMake builds the optional bundled C++ kernels and combines them for native consumers:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release \
  -DLMFLOW_BUILD_SHARED_LIBS=ON \
  -DLMFLOW_BUILD_KERNELS=ON
cmake --build build
ctest --test-dir build                       # flow.hpp test; CV test if OpenCV is present
cmake --install build --prefix /opt/lmflow   # → headers + lib + lib/cmake/lmflow
```

The switches are independent:

- `LMFLOW_BUILD_SHARED_LIBS=ON` (default): install `liblmflow_core.so` and the complete `liblmflow.so`.
- `LMFLOW_BUILD_SHARED_LIBS=OFF`: expose static CMake targets backed by `liblmflow_core.a` and, when enabled, `liblmflow_kernels.a`.
- `LMFLOW_BUILD_KERNELS=ON` (default): include the 18 bundled C++ kernels in `lmflow::lmflow`.
- `LMFLOW_BUILD_KERNELS=OFF`: build a pure Rust engine only, without bundled C++ kernels.

Consumers choose the desired boundary:

```cmake
find_package(lmflow REQUIRED)
target_link_libraries(my_app PRIVATE lmflow::lmflow)     # core + optional bundled kernels
# target_link_libraries(my_app PRIVATE lmflow::core)     # selected pure-core variant
# target_link_libraries(my_app PRIVATE lmflow::core_static)
```

> Rust developers use `cargo` in `lmflow/core`; Python users just `pip install lm-lmflow` (prebuilt wheels). The wheel is built by **scikit-build-core driving this same root CMake** (`-DLMFLOW_BUILD_PYTHON=ON`), so there is one build definition, not three.

Asyncio applications can consume typed output events without polling:

```python
events = graph.events("out")
async for event in events:
    if isinstance(event, lmflow.PacketEvent):
        handle(event.packet)
    elif isinstance(event, lmflow.TimestampBoundEvent):
        advance_watermark(event.timestamp)
    else:  # DoneEvent
        break
```

`events()` starts the graph on first iteration and shares the same event-loop wakeup driver as
`graph.run_async()`, so multiple output ports can be consumed concurrently.

The C ABI is the only stable interface (`lmflow/flow.h`). `flow.hpp` adds header-only C++ RAII
host wrappers (`lmflow::Graph`, `lmflow::Input`, `lmflow::Poller`) with Rust-compatible method
names, plus the existing C++ kernel authoring sugar. `flow_platform_log.hpp` bridges engine logs
to the platform logger (logcat / os_log / HiLog). OpenCV interop is a separate opt-in component
under `adapters/opencv`, exposed as `<lmflow/opencv.hpp>` and the CMake target `lmflow::opencv`.

Mobile integration examples: [`lmflow/examples/android/hello_world`](lmflow/examples/android/hello_world) (JNI), [`lmflow/examples/ios/hello_world`](lmflow/examples/ios/hello_world) (Swift), [`lmflow/examples/harmonyos/hello_world`](lmflow/examples/harmonyos/hello_world) (NAPI).

The asyncio production host example is [`lmflow/examples/python/async_pipeline`](lmflow/examples/python/async_pipeline). It demonstrates event-loop wakeups, typed output events, timeout cancellation, and graceful cleanup.

## Performance benchmarks

Benchmarks are split by boundary so measuring C++ never adds a second C++ build
path to the pure Rust core:

```bash
cd lmflow/core
cargo bench --bench dispatch    # scheduler/executor dispatch
cargo bench --bench throughput  # Rust Input → Graph → Poller
cargo bench --bench packet      # Packet clone and copy-on-write
```

See [`lmflow/benchmarks/README.md`](lmflow/benchmarks/README.md) for complete
C/C++ SDK and Python/NumPy binding benchmarks. CI compiles benchmark targets but
does not enforce noisy timing thresholds on shared runners.

## Documentation

<https://laomou.github.io/lm-flow/> — one site, three API surfaces:

| Surface | Reference | Generated from |
|---|---|---|
| Rust | [`/rust/`](https://laomou.github.io/lm-flow/rust/) (tracks `main`) · [docs.rs/lmflow](https://docs.rs/lmflow) (released version) | doc comments in `lmflow/core/src/` |
| C / C++ | [`/cpp/`](https://laomou.github.io/lm-flow/cpp/) — hand-written guide | `lmflow/include/lmflow/flow.h`, the authoritative ABI definition |
| Python | [`/python/`](https://laomou.github.io/lm-flow/python/) | docstrings in `lmflow/python/lmflow/__init__.py` |

The design document — scheduling model, timestamp and termination semantics, lock ordering rules and the decision log — is [`docs/design.md`](docs/design.md) (Chinese, authoritative), also rendered at [`/design/`](https://laomou.github.io/lm-flow/design/).

Release notes are in [`CHANGELOG.md`](CHANGELOG.md).

The site is built and deployed by [`.github/workflows/docs.yml`](.github/workflows/docs.yml) on every push to `main`; the hand-written pages live in [`docs/web/`](docs/web). To preview it locally:

```bash
pip install pdoc                                    # also brings markdown2 + pygments
cargo doc --no-deps --manifest-path lmflow/core/Cargo.toml
python docs/web/build.py site
cp -R lmflow/core/target/doc/. site/rust/ && rm -f site/rust/.lock
python -m pdoc lmflow -o site/python                # needs `pip install .` first
python -m http.server -d site 8000
```
