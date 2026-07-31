# lm-flow

一个数据流图计算框架:把计算描述成**有向图**,节点是**算子(Kernel)**,边上流动**带时间戳的数据包(Packet)**。
引擎用 Rust 实现(调度、线程、队列、拓扑),对外只暴露一层稳定的 **C ABI**;算子可以用 **C++** 或 **Python** 编写。

```text
              ┌─────────────────────────────────────────────┐
  宿主(驱动图)│  Rust / C++ / Python                        │
              ├──────────────── C ABI (include/flow.h) ─────┤
  引擎        │  Rust:调度器 · 执行器 · 边队列 · 拓扑 · YAML │
              ├──────────────── C ABI (回调) ───────────────┤
  算子        │  C++(flow.hpp 糖层) / Python                │
              └─────────────────────────────────────────────┘
```

## 目录结构

```text
lm-flow/
├── include/                   公共头(C ABI 权威定义 + 可选 C++ 糖层)
│   ├── flow.h                 C ABI —— 唯一稳定接口
│   └── flow.hpp               C++ 算子糖层(header-only,非 ABI)
├── cpp/                       C++ 算子
│   ├── kernels.cc             内置示例算子集(8 个,覆盖不同用途)
│   └── abi_assert.cc          跨界结构体布局的编译期校验
├── crates/
│   ├── flow-core/             引擎(lib + staticlib + cdylib)
│   │   ├── build.rs           用 cc 编译 cpp/ 并链入
│   │   ├── src/
│   │   ├── tests/             含 ABI 布局一致性测试
│   │   └── examples/          Rust 宿主示例
│   └── flow-py/               Python 绑定
├── python/lmflow/             Python 包
├── examples/
│   ├── cpp/                   外部 C++ 宿主示例(不进 cargo 构建)
│   └── python/                Python 示例
└── docs/design.md             设计方案(权威文档)
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
| `Poller/Observer` | 图输出的两种取法:拉(阻塞/超时/非阻塞)与推(回调) |

## 快速开始

```bash
cargo build                       # 编译引擎 + C++ 算子
cargo test                        # 单测 + ABI 布局一致性
cargo run --example hello_world   # 两级直通管线,输出 0..9
```

算子长这样(C++,用糖层):

```cpp
class PassThroughKernel : public flow::Kernel {
 public:
  static void GetContract(flow::Contract& c) { c.InputSetAny(0); c.OutputSetAny(0); }
  flow::Status Process(flow::Context& cc) override {
    cc.Forward(0, 0);                      // 零拷贝直通
    return flow::Status::Ok();
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
