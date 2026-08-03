# lm-flow

**English** | [简体中文](README.zh-CN.md)

A dataflow-graph engine: computation is described as a **directed graph** — nodes are **kernels**, and **timestamped packets** flow along the edges. The engine is written in Rust (scheduling, threads, queues, topology) and exposes a single stable **C ABI**; kernels can be written in **C++** or **Python**.

📖 **API reference (Python)**: <https://laomou.github.io/lm-flow/> — auto-generated from docstrings.

```text
  Host (Rust / C++ / Python) ── drives the graph
        │  C ABI  (lmflow/flow.h)
        ▼
  Engine (Rust): scheduler · executors · edge queues · topology · YAML
        │  C ABI  (callbacks)
        ▼
  Kernels: C++ (flow.hpp sugar)  /  Python
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
│       ├── cpp/hello_world/    standalone C++ project (find_package or build-from-source)
│       ├── python/             hello_world/, realtime_pipeline/, opencv_pipeline/
│       └── {android,ios,harmonyos}/hello_world/   mobile integration examples
├── third_party/pybind11/      vendored git submodule (only used to build the Python wheel)
├── cmake/                     engine.cmake · install-sdk.cmake · find_package config
├── docs/design.md             Design document (authoritative)
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
| `Poller/Observer` | Two ways to take graph output: pull (blocking/timeout/non-blocking) and push (callback) |

## Quick start

The engine is a standalone Rust crate under `lmflow/core` (Rust developers work there directly):

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
lmflow-v0.1.0-linux-x86_64/
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
