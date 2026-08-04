# lm-flow

[English](README.md) | **简体中文**

[![crates.io](https://img.shields.io/crates/v/lmflow.svg?logo=rust)](https://crates.io/crates/lmflow)
[![PyPI](https://img.shields.io/pypi/v/lm-lmflow.svg?logo=pypi&logoColor=white)](https://pypi.org/project/lm-lmflow/)
[![docs](https://img.shields.io/badge/docs-lm--flow-blue)](https://laomou.github.io/lm-flow/)
[![ci](https://github.com/laomou/lm-flow/actions/workflows/ci.yml/badge.svg)](https://github.com/laomou/lm-flow/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

一个数据流图计算框架:把计算描述成**有向图**,节点是**算子(Kernel)**,边上流动**带时间戳的数据包(Packet)**。
引擎用 Rust 实现(调度、线程、队列、拓扑),对外只暴露一层稳定的 **C ABI**;算子可以用 **Rust**、**C++** 或 **Python** 编写。

📖 **文档**:<https://laomou.github.io/lm-flow/> —— [Rust](https://laomou.github.io/lm-flow/rust/) · [C / C++](https://laomou.github.io/lm-flow/cpp/) · [Python](https://laomou.github.io/lm-flow/python/)

```text
  宿主(Rust / C++ / Python) ── 驱动图
        │  C ABI  (lmflow/flow.h)
        ▼
  引擎(Rust):调度器 · 执行器 · 边队列 · 拓扑 · YAML
        │  C ABI  (回调)
        ▼
  算子:Rust(trait Kernel) / C++(flow.hpp 糖层) / Python
```

## 目录结构

第一方源码都在 `lmflow/` 下;构建文件与 vendored 子模块在仓库根(仓库根同时也是 Python 项目根,
打 wheel 时需要的东西都在项目内,不会丢)。

```text
lm-flow/
├── lmflow/                    第一方源码
│   ├── core/                  引擎 —— `lmflow` crate,**纯 Rust**(包名=库名=lmflow → liblmflow.a)
│   │   ├── build.rs           可选地用 cc 编 ../cpp(feature builtin-kernels,默认关)
│   │   ├── Cargo.toml · Cargo.lock
│   │   └── src/ · tests/ · benches/
│   ├── include/lmflow/        公共头 —— flow.h(C ABI,唯一稳定接口)· flow.hpp(C++ 算子糖层)· flow_cv.hpp · flow_platform_log.hpp
│   ├── cpp/                   C++ 侧(非引擎)—— kernels/(18 个内置算子)· abi_assert.cc · tests/
│   ├── python/                pybind11 绑定(src/)+ lmflow 包 + CMakeLists
│   └── examples/              每个示例是独立工程:examples/<lang>/<name>/
│       ├── cpp/               hello_world/、custom_type/(find_package 或从源码构建)
│       ├── rust/              hello_world/(独立 cargo 工程)
│       ├── python/            hello_world/、realtime_pipeline/、opencv_pipeline/
│       └── {android,ios,harmonyos}/hello_world/   移动端集成示例
├── third_party/pybind11/      vendored git 子模块(仅用于构建 Python wheel)
├── cmake/                     engine.cmake · install-sdk.cmake · find_package 配置
├── docs/                      design.md(设计方案,权威文档,中文)· web/(文档站源码)
├── CHANGELOG.md               发布记录(crate · wheel · 原生 SDK 共用一个版本号)
├── CMakeLists.txt             顶层构建(驱动 cargo;C/C++ SDK + Python 扩展)
└── pyproject.toml             Python wheel(scikit-build-core → 同一份 CMake)
```

## 核心概念

| 概念 | 说明 |
|---|---|
| `Graph` | 一张计算图,由 YAML 描述;负责初始化、启动、关流、终止 |
| `Node` | 图中一个节点,持有一个算子实例 |
| `Kernel` | 算子 —— 用户编写的计算逻辑,实现 `Open/Process/Close` |
| `Edge/Port` | 边与端口,按名字连接上下游;边上是带时间戳的包队列 |
| `Packet` | 数据包 = 不可变共享的 payload + 时间戳 |
| `Contract` | 端口类型契约,算子在 `GetContract` 中声明 |
| `Poller/Observer` | 图输出拉/推；有界 Poller 支持阻塞、丢弃和仅保留最新值 |

具体类型契约会在建图期沿边检查:已知的 `I64 → F64` 连接直接报错;任一侧声明
`any` 时保留运行期逐包检查。算子实际输出也必须符合自身声明,不会因直接连接图输出而绕过。

## 快速开始

Rust —— 直接从 crates.io 装,**不需要 C++ 工具链**:

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

register_kernel::<Double>("Double")?;          // 自己的算子
let g = Graph::from_yaml(yaml)?;               // 自带的 `PassThrough` / `Sink` 无需注册
```

发布出去的 crate 是**纯 Rust 引擎**;18 个内置 C++ 算子在 crate 之外、不随它分发 ——
在本仓库里构建才有(见下面的 `builtin-kernels` feature)。

在本仓库里开发(引擎 crate 在 `lmflow/core`):

```bash
cd lmflow/core
cargo build     # 纯 Rust 引擎,不编任何 C++
cargo test      # 单测 + ABI 布局 + Rust 算子测试

# 18 个内置 C++ 算子在 crate 之外(lmflow/cpp),要显式开:
cargo build --features builtin-kernels
cargo test  --features builtin-kernels   # 全量套件(C ABI / 内存 / 策略 …)
cargo bench --features builtin-kernels   # Criterion 吞吐 → target/criterion/
```

Python:

```bash
pip install lm-lmflow               # 预编译 wheel(Linux manylinux / macOS)
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

> 只发布预编译 wheel —— 装不到匹配平台的 wheel 时,`pip install` 直接失败,而**不会**在你机器上编译源码。
> 想本地从源码构建,就带子模块克隆再装(需要 Rust 工具链 + CMake):
>
> ```bash
> git clone --recursive https://github.com/laomou/lm-flow
> pip install ./lm-flow          # scikit-build-core 驱动 CMake → cargo + pybind11
> ```

算子长这样(C++,用糖层):

```cpp
class PassThroughKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) { c.InputSetAny(0); c.OutputSetAny(0); }
  lmflow::Status Process(lmflow::Context& cc) override {
    cc.Forward(0, 0);                      // 零拷贝直通
    return lmflow::Status::Ok();
  }
};
```

图长这样(YAML):

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

详细设计见 [`docs/design.md`](docs/design.md)。

## 原生 SDK(C / C++ / 移动端)

C/C++ 或移动端宿主不走 pip —— 直接用**头文件 + 库**。每个 tag 的 GitHub Release 会附带各平台的
`lmflow-<版本>-<平台>.tar.gz`(Linux x86_64/aarch64、macOS arm64、iOS arm64、Android arm64):

```text
lmflow-v0.3.0-linux-x86_64/
├── include/lmflow/   flow.h · flow.hpp · flow_cv.hpp · flow_platform_log.hpp
└── lib/       liblmflow.a(静态,完整,首选)· liblmflow.so(动态)
```

```bash
# 链静态库(推荐,尤其移动端嵌入):
g++ -std=c++17 -Iinclude my_host.cc lib/liblmflow.a -lpthread -ldl -lm -o my_host
```

本地自己出一份也行:

```bash
cd lmflow/core
cargo build --release --features builtin-kernels   # → lmflow/core/target/release/liblmflow.{a,so}
# 头文件在 lmflow/include/lmflow 下
```

### 用 CMake 构建与消费

C++/原生侧的顶层构建是 CMake,它在仓库根,**驱动 cargo**(由 cargo 把 Rust 引擎 + C++ 算子编成 `liblmflow`),再编 C++ 示例/测试,并安装出 `find_package` 配置:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
ctest --test-dir build                       # flow.hpp 测试;装了 OpenCV 则含 CV 测试
cmake --install build --prefix /opt/lmflow   # → headers + lib + lib/cmake/lmflow
```

消费者只需:

```cmake
find_package(lmflow REQUIRED)
target_link_libraries(my_app PRIVATE lmflow::core)   # 头 + liblmflow.a + 系统库
```

> Rust 开发者在 `lmflow/core` 里用 `cargo`;Python 用户直接 `pip install lm-lmflow`(预编 wheel)。
> wheel 由 **scikit-build-core 驱动这同一份根 CMake**(`-DLMFLOW_BUILD_PYTHON=ON`)构建 —— 一份构建定义,而非三份。

C ABI 是唯一稳定接口(`lmflow/flow.h`);`flow.hpp` 是可选的 C++ 算子糖层,`flow_cv.hpp` 是 OpenCV 互转,`flow_platform_log.hpp` 一行把引擎日志接到平台日志系统(logcat / os_log / HiLog)—— `lmflow::InstallPlatformLogSink()`。

移动端集成示例:[`lmflow/examples/android/hello_world`](lmflow/examples/android/hello_world)(JNI)、[`lmflow/examples/ios/hello_world`](lmflow/examples/ios/hello_world)(Swift)、[`lmflow/examples/harmonyos/hello_world`](lmflow/examples/harmonyos/hello_world)(NAPI)。

## 文档

<https://laomou.github.io/lm-flow/> —— 一个站点,三端 API:

| 端 | 文档 | 来源 |
|---|---|---|
| Rust | [`/rust/`](https://laomou.github.io/lm-flow/rust/)(跟 `main`)· [docs.rs/lmflow](https://docs.rs/lmflow)(已发布版本) | `lmflow/core/src/` 的 doc 注释 |
| C / C++ | [`/cpp/`](https://laomou.github.io/lm-flow/cpp/) —— 手写指南 | `lmflow/include/lmflow/flow.h`,ABI 的权威定义 |
| Python | [`/python/`](https://laomou.github.io/lm-flow/python/) | `lmflow/python/lmflow/__init__.py` 的 docstring |

设计方案 —— 调度模型、时间戳与终止语义、锁序规则、决策记录 —— 是 [`docs/design.md`](docs/design.md)(中文,权威),同时渲染在 [`/design/`](https://laomou.github.io/lm-flow/design/)。

发布记录见 [`CHANGELOG.md`](CHANGELOG.md)。

站点由 [`.github/workflows/docs.yml`](.github/workflows/docs.yml) 在每次推 `main` 时构建部署;手写页面的源码在 [`docs/web/`](docs/web)。本地预览:

```bash
pip install pdoc                                    # 顺带带来 markdown2 + pygments
cargo doc --no-deps --manifest-path lmflow/core/Cargo.toml
python docs/web/build.py site
cp -R lmflow/core/target/doc/. site/rust/ && rm -f site/rust/.lock
python -m pdoc lmflow -o site/python                # 需先 `pip install .`
python -m http.server -d site 8000
```
