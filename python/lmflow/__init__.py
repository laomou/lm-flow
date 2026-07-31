"""lmflow —— 数据流图计算框架的 Python 接口。

分两层:
  * 原生扩展 ``lmflow._lmflow``(pybind11 编写,链接引擎)提供实际能力;
  * 本模块提供 Python 侧的糖:``@kernel`` 装饰器、``Kernel`` 基类、类型标注。

两件必须知道的事
----------------
**GIL**:Python 算子的 ``process`` 由引擎工作线程回调,期间持有 GIL,因此多个
Python 算子之间**无法真并行**。重计算请写成 C++ 算子。所有可能阻塞的接口
(``poller.next`` / ``wait_done`` / ``send``)在等待期间都会释放 GIL,否则工作
线程拿不到 GIL 会直接死锁。

**数据类型**:Python 算子只能收发**内建类型** —— 整数、浮点、布尔、字符串、
bytes,以及 N 维缓冲(``as_numpy()`` / ``new_buffer()``)。不支持把 dict、list
或自定义类实例直接放进数据流,因为那样的包只能在纯 Python 子图里流动,接到 C++
算子上就成了无法解读的指针。结构化数据请用:数值集合 → N×K 的 numpy 缓冲
(零拷贝,C++ 侧可直读);任意元数据 → JSON 字符串;配置参数 → node ``options``。

**生命周期**:图必须在解释器开始销毁**之前**停掉,否则工作线程可能回调进一个
正在析构的解释器而崩溃。请始终用 ``with lmflow.Graph.from_yaml(...) as g:``,
或显式调用 ``g.close()``;``__del__`` 只作兜底,不保证时机。
"""

from __future__ import annotations

from typing import Any, Callable, Sequence

try:
    from . import _lmflow as _native
except ImportError as exc:  # pragma: no cover
    raise ImportError(
        "lmflow 的原生扩展未构建。请先运行 `maturin develop` 或 `pip install -e .`。"
    ) from exc

__all__ = [
    "Kernel",
    "kernel",
    "Graph",
    "Packet",
    "Contract",
    "Context",
    "Input",
    "Poller",
    "FlowError",
    "Timeout",
]

# 从原生扩展直接复用的类型
Graph = _native.Graph
Packet = _native.Packet
Contract = _native.Contract
Context = _native.Context
Input = _native.Input
Poller = _native.Poller
FlowError = _native.FlowError
Timeout = _native.Timeout


class Kernel:
    """Python 算子基类。

    子类实现 ``process``(必需),可选实现 ``open`` / ``close``,
    以及可选的静态 ``get_contract(c)`` 用于声明端口类型。

    实例的生命周期由引擎管理:每个图节点一个实例,``open`` 在图启动时调用一次,
    ``process`` 每个数据包调用一次,``close`` 在关流时调用一次。
    """

    def open(self, cc: Context) -> None:  # noqa: D102
        """图启动时调用一次。适合读 options、准备资源。"""

    def process(self, cc: Context) -> None:  # noqa: D102
        """每个数据包调用一次。必须实现。"""
        raise NotImplementedError

    def close(self, cc: Context) -> None:  # noqa: D102
        """关流时调用一次。可在此产出汇总结果。"""


def kernel(name: str) -> Callable[[type], type]:
    """把一个 :class:`Kernel` 子类注册到引擎,供 YAML 里按 ``name`` 引用。

    >>> @lmflow.kernel("MyKernel")
    ... class MyKernel(lmflow.Kernel):
    ...     def process(self, cc):
    ...         cc.forward(0, 0)

    注册发生在**装饰器执行时**(即模块 import 时),所以务必在
    ``Graph.from_yaml`` 之前完成 import。同名重复注册会抛 :class:`FlowError`。
    """

    def decorator(cls: type) -> type:
        if not issubclass(cls, Kernel):
            raise TypeError(f"{cls.__name__} 必须继承 lmflow.Kernel")
        _native.register_kernel(name, cls)
        return cls

    return decorator


def registered_kernels() -> Sequence[str]:
    """列出当前已注册的算子名(含 C++ 内置算子)。"""
    return _native.registered_kernels()


def set_log_callback(fn: Callable[[int, str], Any] | None) -> None:
    """设置日志回调 ``fn(level, message)``;传 ``None`` 恢复静默。

    回调可能在任意工作线程被调用(调用时引擎不持有内部锁)。
    """
    _native.set_log_callback(fn)
