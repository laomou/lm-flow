"""lmflow —— 数据流图计算框架的 Python 接口。

把计算描述成**有向图**:节点是**算子(Kernel)**,边上流动**带时间戳的数据包(Packet)**。
算子可以用 Python 写,也可以用 C++ 写,在 YAML 里平等引用 —— 引擎不区分。

最小例子
--------
>>> import lmflow
>>> @lmflow.kernel("Double")
... class Double(lmflow.Kernel):
...     def process(self, cc):
...         cc.emit(0, cc.input(0).as_int() * 2)
>>> with lmflow.Graph.from_yaml('''
... nodes:
...   - { name: d, kernel: Double, input_ports: [in], output_ports: [out] }
... input_ports: [in]
... output_ports: [out]
... ''') as g:
...     out = g.add_poller("out")
...     g.start()
...     g.input("in").send(21, ts=0)
...     print(out.next().as_int())
42

三件必须知道的事
----------------
**生命周期** —— 图必须在解释器开始销毁**之前**停掉,否则引擎线程可能回调进一个正在
析构的解释器而崩溃。请始终用 ``with lmflow.Graph.from_yaml(...) as g:``,或显式
``g.close()``。``__del__`` 只作兜底,不保证时机。

**GIL** —— 节点未指定 ``executor`` 时跑在**宿主主线程**上,此时 Python 算子之间
根本不存在 GIL 争抢。只有显式把 Python 算子放进线程池才需要考虑:那时它们无法真并行,
重计算应交给 C++ 算子(或让 Python 算子留在主线程、把 C++ 算子放进池里)。
所有可能阻塞的接口(``poller.next`` / ``wait_done`` / ``send``)在等待期间都会释放 GIL。

**数据类型** —— Python 算子只能收发**内建类型**:int / float / bool / str / bytes,
以及 N 维数值缓冲(``as_numpy()`` / ``new_buffer()``)。不支持把 dict、list 或自定义
类实例直接放进数据流 —— 那样的包只能在纯 Python 子图里流动,接到 C++ 算子上就成了
无法解读的指针。结构化数据请用:数值集合 → N×K 的 numpy 缓冲(零拷贝,C++ 侧可直读);
任意元数据 → JSON 字符串;配置参数 → node ``options``。
"""

from __future__ import annotations

import os
import sys
from typing import Any, Callable, Iterator, Sequence

try:
    from . import _lmflow as _native
except ImportError as exc:  # pragma: no cover
    raise ImportError(
        "lmflow 的原生扩展未构建。请运行 `python python/build.py`(或 pip install -e .)。"
    ) from exc

__all__ = [
    "Kernel",
    "kernel",
    "Graph",
    "Packet",
    "Context",
    "Contract",
    "Input",
    "Poller",
    "LogLevel",
    "CloseReason",
    "GraphState",
    "Timeout",
    "registered_kernels",
    "set_log_callback",
    "TS_UNSET",
    "TS_PRE_STREAM",
    "TS_POST_STREAM",
    "TS_DONE",
    "INVALID_ID",
]

# 从原生扩展直接复用的类型
Packet = _native.Packet
Context = _native.Context
Contract = _native.Contract
Input = _native.Input
Poller = _native.Poller

TS_UNSET = _native.TS_UNSET
TS_PRE_STREAM = _native.TS_PRE_STREAM
TS_POST_STREAM = _native.TS_POST_STREAM
TS_DONE = _native.TS_DONE
INVALID_ID = _native.INVALID_ID


class Timeout(TimeoutError):
    """带超时的接口在超时时抛出。"""


class LogLevel:
    ERROR = 0
    WARN = 1
    INFO = 2
    DEBUG = 3


class CloseReason:
    """算子 ``close`` 的触发原因 —— 据此决定是否提交结果。"""

    NORMAL = _native.CLOSE_NORMAL
    ERROR = _native.CLOSE_ERROR
    CANCELLED = _native.CLOSE_CANCELLED


class GraphState:
    CREATED = 0
    INITIALIZED = 1
    RUNNING = 2
    DRAINING = 3
    TERMINATED = 4


# ---------------------------------------------------------------- 算子


class Kernel:
    """Python 算子基类。

    子类实现 ``process``(必需),可选实现 ``open`` / ``close``,
    以及可选的静态 ``get_contract(c)`` 用于声明端口类型与必需的 side packet。

    生命周期由引擎管理:每个图节点一个实例;``open`` 在图启动时调用一次,
    ``process`` 每个数据包(或每个对齐后的时刻)调用一次,``close`` 在关流时调用一次。

    方法里抛出的异常会被捕获、转成算子失败,并把异常文本并入图级错误 ——
    不会穿越 FFI 边界导致崩溃。
    """

    def open(self, cc: Context) -> None:
        """图启动时调用一次。适合读 options、准备资源。"""

    def process(self, cc: Context) -> None:
        """每个数据包调用一次。必须实现。"""
        raise NotImplementedError(
            f"{type(self).__name__} 必须实现 process(self, cc)"
        )

    def close(self, cc: Context) -> None:
        """关流时调用一次。可在此产出汇总结果 —— 但先看 ``cc.close_reason``。"""


def kernel(name: str) -> Callable[[type], type]:
    """把一个 :class:`Kernel` 子类注册到引擎,供 YAML 里按 ``name`` 引用。

    注册发生在**装饰器执行时**(即模块 import 时),所以务必在
    :meth:`Graph.from_yaml` 之前完成 import。同名重复注册会抛异常。
    """

    def decorator(cls: type) -> type:
        if not issubclass(cls, Kernel):
            raise TypeError(f"{cls.__name__} 必须继承 lmflow.Kernel")
        _native.register_kernel(name, cls)
        return cls

    return decorator


def registered_kernels() -> Sequence[str]:
    """已注册的算子名(含 C++ 内置算子)。"""
    return _native.registered_kernels()


def register_builtin_kernels() -> None:
    """注册捆绑的 C++ 内置算子(幂等)。必须在建图之前调用。"""
    _native.register_builtin_kernels()


def set_log_callback(fn: Callable[[int, str], Any] | None) -> None:
    """设置日志回调 ``fn(level, message)``;传 ``None`` 恢复静默。

    回调可能在任意工作线程被调用(调用时引擎不持有内部锁)。
    """
    _native.set_log_callback(fn)


def type_name(type_id: int) -> str:
    """type_id 的可读名字(用于诊断)。"""
    return _native.type_name(type_id)


# ---------------------------------------------------------------- 图


class Graph:
    """一张计算图。

    **务必用作上下文管理器**:图需要在解释器销毁前停掉(见模块文档)。
    """

    def __init__(self) -> None:
        self._g = _native.Graph()
        self._closed = False

    # ---- 构造 ----

    @classmethod
    def from_yaml(cls, text: str) -> Graph:
        g = cls()
        g._g.init_from_yaml(text)
        return g

    @classmethod
    def from_yaml_file(cls, path: str | os.PathLike[str]) -> Graph:
        g = cls()
        g._g.init_from_yaml_file(os.fspath(path))
        return g

    # ---- 上下文管理 ----

    def __enter__(self) -> Graph:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def close(self) -> None:
        """停掉图并释放资源。幂等。"""
        if not self._closed:
            self._closed = True
            self._g.close()

    def __del__(self) -> None:  # pragma: no cover - 兜底,时机不保证
        try:
            self.close()
        except Exception:
            pass

    # ---- 启动前 ----

    def set_side_packet(self, name: str, value: Any) -> None:
        """注入常量输入(模型句柄、标定参数…)。必须在 :meth:`start` 之前。"""
        self._g.set_side_packet(name, value)

    def add_poller(self, port: str) -> Poller:
        """拉模式订阅一个图输出口。必须在 :meth:`start` 之前。"""
        return self._g.add_poller(port)

    def observe(self, port: str, fn: Callable[[Packet], Any]) -> None:
        """推模式订阅。回调在**派发该包的线程**上执行,包为借用、回调返回后失效。"""
        self._g.observe(port, fn)

    def start(self) -> None:
        self._g.start()

    # ---- 运行时 ----

    def input(self, port: str) -> Input:
        """取图输入口句柄(热路径免按名字查表)。"""
        return self._g.input(port)

    def send(self, port: str, value: Any, ts: int | None = None) -> None:
        """便捷送包(内部查表)。高频场景请改用 :meth:`input` 拿句柄。"""
        self._g.input(port).send(value, ts)

    def new_buffer(self, shape: Sequence[int], dtype: Any) -> tuple[Packet, Any]:
        """让**引擎**分配缓冲,返回 ``(packet, 可写 numpy 视图)``。

        这是零拷贝的推荐入口:直接把结果写进引擎内存,避免
        ``send(ndarray)`` 那次整帧拷贝,也避免引擎持有 PyObject(见模块文档的 GIL 一节)。
        """
        return self._g.new_buffer(list(shape), dtype)

    def close_input(self, port: str) -> None:
        self._g.close_input(port)

    def close_all_inputs(self) -> None:
        self._g.close_all_inputs()

    def cancel(self) -> None:
        """立即取消:停止调度、丢弃在途包。**不会中断**已在执行的算子回调。"""
        self._g.cancel()

    def pause(self) -> None:
        self._g.pause()

    def resume(self) -> None:
        self._g.resume()

    def wait_done(self, timeout: float | None = None) -> None:
        """等待图跑完(需先关闭输入口)。等待期间会释放 GIL。"""
        try:
            self._g.wait_done(timeout)
        except TypeError as e:  # 原生层用 TypeError 表示超时
            raise Timeout(str(e)) from None

    def wait_until_idle(self, timeout: float | None = None) -> None:
        """等到在途包都处理完,但**不结束图**(批处理模式)。"""
        try:
            self._g.wait_until_idle(timeout)
        except TypeError as e:
            raise Timeout(str(e)) from None

    # ---- 内省 ----

    @property
    def state(self) -> int:
        return self._g.state

    def dump(self) -> str:
        """拓扑与状态的可读快照(节点表含 running/耗时,便于定位卡死)。"""
        return self._g.dump()

    def last_error(self) -> str:
        """图级错误文本 —— 工作线程上算子的失败原因只能从这里拿到。"""
        return self._g.last_error()

    def queue_depth(self, port: str) -> int:
        return self._g.queue_depth(port)

    def dropped_count(self, port: str) -> int:
        """该边累计被丢弃的包数(仅 fixed_size 策略会丢)。"""
        return self._g.dropped_count(port)

    def counter_value(self, name: str) -> int:
        """算子自报计数器的当前值。"""
        return self._g.counter_value(name)

    def total_queued(self) -> int:
        return self._g.total_queued()

    def node_names(self) -> list[str]:
        return self._g.node_names()

    def node_stats(self, index: int) -> dict[str, Any]:
        """节点统计:running / running_for_us / processed / errors / 耗时 / queued。"""
        return self._g.node_stats(index)

    def __repr__(self) -> str:
        names = "?" if self._closed else ",".join(self.node_names())
        return f"<lmflow.Graph nodes=[{names}] state={self.state}>"


def _poller_iter(self: Poller) -> Iterator[Packet]:
    """让 poller 可以直接 for 循环:图结束时自然停止。"""
    while True:
        pkt = self.next()
        if pkt is None:
            return
        yield pkt


Poller.__iter__ = _poller_iter  # type: ignore[attr-defined]

# 版本信息
ABI_VERSION = _native.ABI_VERSION
__version__ = "0.1.0"

if sys.version_info < (3, 8):  # pragma: no cover
    raise RuntimeError("lmflow 需要 Python 3.8+")
