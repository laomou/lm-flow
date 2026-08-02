# lm-flow

**English** | [简体中文](README.zh-CN.md)

A dataflow-graph engine: computation is described as a **directed graph** — nodes are **kernels**, and **timestamped packets** flow along the edges. The engine is written in Rust (scheduling, threads, queues, topology) and exposes a single stable **C ABI**; kernels can be written in **C++** or **Python**.

```text
  Host (Rust / C++ / Python) ── drives the graph
        │  C ABI  (include/flow.h)
        ▼
  Engine (Rust): scheduler · executors · edge queues · topology · YAML
        │  C ABI  (callbacks)
        ▼
  Kernels: C++ (flow.hpp sugar)  /  Python
```

## Layout

```text
lm-flow/
├── include/                   Public headers (authoritative C ABI + optional C++ sugar)
│   ├── flow.h                 C ABI — the only stable interface
│   └── flow.hpp               C++ kernel sugar (header-only, not ABI)
├── cpp/                       C++ kernels
│   ├── kernels/               Built-in sample kernels (11, one file per kernel + register.cc)
│   ├── abi_assert.cc          Compile-time checks of the cross-boundary struct layout
│   └── tests/                 C++ test executables (flow.hpp unit test, CV conversion test)
├── flow-core/                 Engine — the Rust crate (lib + staticlib + cdylib)
│   ├── build.rs               Compiles cpp/ via `cc` and links it in
│   ├── src/
│   ├── tests/                 Includes ABI layout-consistency tests
│   └── examples/              Rust host examples
├── python/
│   ├── src/bindings.cc        Python bindings (pybind11)
│   ├── lmflow/                Python package (pip install lm-flow → import lmflow)
│   └── build.py               pip-free local build script
├── examples/                  each example is a self-contained project: examples/<lang>/<name>/
│   ├── cpp/hello_world/       standalone C++ project (CMake: find_package or build-from-source)
│   ├── python/                hello_world/, realtime_pipeline/, opencv_pipeline/
│   └── {android,ios,harmonyos}/hello_world/   mobile integration examples
└── docs/design.md             Design document (authoritative)
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

```bash
cargo build                       # build the engine + C++ kernels
cargo test                        # unit tests + ABI layout consistency
cargo run --example hello_world   # two-stage passthrough pipeline, prints 0..9
```

Python:

```bash
pip install lm-flow               # prebuilt wheels (Linux manylinux / macOS)
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

> When there is no prebuilt wheel for your platform, `pip` builds from source — you'll need a Rust toolchain and a C++ compiler. You can also skip pip entirely: `python python/build.py` builds the extension in place.

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

C/C++ and mobile hosts don't use pip — they use the **headers + library** directly. Each tagged GitHub Release ships per-platform `lmflow-<version>-<platform>.tar.gz` (Linux x86_64/aarch64, macOS arm64/x86_64, iOS arm64, Android arm64):

```text
lmflow-v0.1.0-linux-x86_64/
├── include/   flow.h · flow.hpp · flow_cv.hpp · flow_platform_log.hpp
└── lib/       libflow_core.a (static, self-contained, preferred) · libflow_core.so (shared)
```

```bash
# Link the static library (recommended, especially for mobile embedding):
g++ -std=c++17 -Iinclude my_host.cc lib/libflow_core.a -lpthread -ldl -lm -o my_host
```

Or build one yourself locally:

```bash
cargo build --release          # → target/release/libflow_core.{a,so}
# the headers are the three under include/
```

### Build & consume with CMake

CMake is the top-level build for the C++/native side. It **drives cargo** (which builds the Rust engine + C++ kernels into `libflow_core`), builds the C++ examples/tests, and installs a `find_package` config:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
ctest --test-dir build                       # flow.hpp test; CV test if OpenCV is present
cmake --install build --prefix /opt/lmflow   # → headers + lib + lib/cmake/lmflow
```

Consumers then just:

```cmake
find_package(lmflow REQUIRED)
target_link_libraries(my_app PRIVATE lmflow::flow_core)   # headers + libflow_core.a + system libs
```

> Rust developers keep using `cargo build`/`cargo test`; Python keeps using pip / `python python/build.py`. CMake doesn't replace them — it orchestrates cargo for the C++/SDK path.

The C ABI is the only stable interface (`include/flow.h`); `flow.hpp` is the optional C++ kernel sugar, `flow_cv.hpp` is OpenCV interop, and `flow_platform_log.hpp` bridges engine logs to the platform logger (logcat / os_log / HiLog) in one call — `lmflow::InstallPlatformLogSink()`.

Mobile integration examples: [`examples/android/hello_world`](examples/android/hello_world) (JNI), [`examples/ios/hello_world`](examples/ios/hello_world) (Swift), [`examples/harmonyos/hello_world`](examples/harmonyos/hello_world) (NAPI).
