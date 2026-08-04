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
│   │   ├── build.rs           Optionally compiles ../cpp via `cc` (feature `builtin-kernels`, off by default)
│   │   ├── Cargo.toml · Cargo.lock
│   │   └── src/ · tests/ · benches/
│   ├── include/lmflow/        Public headers — flow.h (C ABI, only stable interface) · flow.hpp (C++ kernel sugar) · flow_cv.hpp · flow_platform_log.hpp
│   ├── cpp/                   C++ side (not the engine) — kernels/ (18 built-ins) · abi_assert.cc · tests/
│   ├── python/                pybind11 bindings (src/) + the lmflow package + CMakeLists
│   └── examples/              each example is self-contained: examples/<lang>/<name>/
│       ├── cpp/               hello_world/, custom_type/  (find_package or build-from-source)
│       ├── rust/              hello_world/  (a standalone cargo project)
│       ├── python/            hello_world/, realtime_pipeline/, opencv_pipeline/
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

register_kernel::<Double>("Double")?;          // your own kernel
let g = Graph::from_yaml(yaml)?;               // `PassThrough` / `Sink` need no registration
```

The published crate is the **pure-Rust engine**. The 18 bundled C++ kernels live outside it and are
not distributed with it — see the `builtin-kernels` feature below if you build from this repo.

Working in this repo instead (the engine crate is `lmflow/core`):

```bash
cd lmflow/core
cargo build     # pure-Rust engine, no C++ compiled
cargo test      # unit tests + ABI layout + Rust-kernel tests

# The 18 bundled C++ kernels live outside the crate (lmflow/cpp) — opt in explicitly:
cargo build --features builtin-kernels
cargo test  --features builtin-kernels   # full suite (C ABI / memory / policies / …)
cargo bench --features builtin-kernels   # Criterion throughput → target/criterion/
```

Python:

```bash
pip install lm-lmflow               # prebuilt wheels (Linux manylinux / macOS)
```

```python
import lmflow

lmflow.register_builtin_kernels()

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

## Native SDK (C / C++ / mobile)

C/C++ and mobile hosts don't use pip — they use the **headers + library** directly. Each tagged GitHub Release ships per-platform `lmflow-<version>-<platform>.tar.gz` (Linux x86_64/aarch64, macOS arm64, iOS arm64, Android arm64):

```text
lmflow-v0.2.0-linux-x86_64/
├── include/lmflow/   flow.h · flow.hpp · flow_cv.hpp · flow_platform_log.hpp
└── lib/       liblmflow.a (static, self-contained, preferred) · liblmflow.so (shared)
```

```bash
# Link the static library (recommended, especially for mobile embedding):
g++ -std=c++17 -Iinclude my_host.cc lib/liblmflow.a -lpthread -ldl -lm -o my_host
```

Or build one yourself locally:

```bash
cd lmflow/core
cargo build --release --features builtin-kernels   # → lmflow/core/target/release/liblmflow.{a,so}
# the headers live under lmflow/include/lmflow
```

### Build & consume with CMake

CMake is the top-level build for the C++/native side; it lives at the repo root and **drives cargo** (which builds the Rust engine + C++ kernels into `liblmflow`), builds the C++ examples/tests, and installs a `find_package` config:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
ctest --test-dir build                       # flow.hpp test; CV test if OpenCV is present
cmake --install build --prefix /opt/lmflow   # → headers + lib + lib/cmake/lmflow
```

Consumers then just:

```cmake
find_package(lmflow REQUIRED)
target_link_libraries(my_app PRIVATE lmflow::core)   # headers + liblmflow.a + system libs
```

> Rust developers use `cargo` in `lmflow/core`; Python users just `pip install lm-lmflow` (prebuilt wheels). The wheel is built by **scikit-build-core driving this same root CMake** (`-DLMFLOW_BUILD_PYTHON=ON`), so there is one build definition, not three.

The C ABI is the only stable interface (`lmflow/flow.h`); `flow.hpp` is the optional C++ kernel sugar, `flow_cv.hpp` is OpenCV interop, and `flow_platform_log.hpp` bridges engine logs to the platform logger (logcat / os_log / HiLog) in one call — `lmflow::InstallPlatformLogSink()`.

Mobile integration examples: [`lmflow/examples/android/hello_world`](lmflow/examples/android/hello_world) (JNI), [`lmflow/examples/ios/hello_world`](lmflow/examples/ios/hello_world) (Swift), [`lmflow/examples/harmonyos/hello_world`](lmflow/examples/harmonyos/hello_world) (NAPI).

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
