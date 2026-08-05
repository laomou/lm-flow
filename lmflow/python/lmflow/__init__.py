"""lmflow — Python interface to the dataflow-graph engine.

Computation is described as a **directed graph**: nodes are **kernels**, and
**timestamped packets** flow along the edges. Kernels may be written in Python
or C++ and are referenced identically in YAML — the engine does not distinguish.

Minimal example
---------------
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

Three things you must know
--------------------------
**Lifecycle** — the graph must be stopped *before* the interpreter begins tearing
down, otherwise engine threads may call back into a dying interpreter and crash.
Always use ``with lmflow.Graph.from_yaml(...) as g:``, or call ``g.close()``
explicitly. ``__del__`` is only a fallback and its timing is not guaranteed.

**GIL** — a node with no ``executor`` runs on the **default executor**, which is a
thread pool sized to the CPU count. So by default Python kernels *do* contend for
the GIL and cannot truly run in parallel; heavy compute belongs in C++ kernels.

To get back the contention-free behaviour, declare a ``DelegatingExecutor`` and point
the Python kernels at it::

    executors:
      - { name: "host", type: "DelegatingExecutor" }
      - { name: "cpu",  type: "ThreadPoolExecutor", num_threads: 4 }
    nodes:
      - { name: resize, kernel: PyResize, executor: "host" }   # Python: no GIL contention
      - { name: invert, kernel: Invert,   executor: "cpu"  }   # C++:真并行

Kernels on ``host`` run serially on whichever host thread enters the engine. In the
usual case where the Python main thread calls ``wait_done`` / ``poller.next`` or
``pump_step``, they run on that main thread and can overlap with C++ kernels on a pool.
The tradeoff is that a delegating executor only advances while a host thread is inside
a blocking call (``wait_done`` / ``wait_until_idle`` / ``poller.next`` / blocking
``send``), or explicitly calls ``pump_step``.

The default executor itself is engine-owned and not configurable — ``default`` is a
reserved name in ``executors``.

Every potentially blocking call (``poller.next`` / ``wait_done`` / ``send``) releases
the GIL while waiting.

**Data types** — Python kernels may only send/receive **builtin types**: int /
float / bool / str / bytes, plus N-dimensional numeric buffers (``as_numpy()`` /
``new_buffer()``). You cannot put a dict, list, or custom class instance straight
into the dataflow — such a packet can only travel within a pure-Python subgraph;
reaching a C++ kernel it becomes an unreadable pointer. For structured data use:
numeric collections → an N×K numpy buffer (zero-copy, readable directly by C++);
arbitrary metadata → a JSON string; config parameters → node ``options``.
"""

from __future__ import annotations

import os
import sys
from typing import Any, Callable, Iterator, Sequence

try:
    from . import _lmflow as _native
except ImportError as exc:  # pragma: no cover
    raise ImportError(
        "lmflow native extension is not built. Run `python python/build.py` (or pip install -e .)."
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
    "DotView",
    "Timeout",
    "registered_kernels",
    "register_builtin_kernels",
    "has_cv_test_kernels",
    "register_cv_test_kernels",
    "type_name",
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
    """Raised by timeout-bearing calls when they time out."""


class LogLevel:
    """Log level constants (matching the C ABI)."""

    ERROR = 0
    WARN = 1
    INFO = 2
    DEBUG = 3


class CloseReason:
    """Why a kernel's ``close`` fired — use it to decide whether to emit results."""

    NORMAL = _native.CLOSE_NORMAL
    ERROR = _native.CLOSE_ERROR
    CANCELLED = _native.CLOSE_CANCELLED


class GraphState:
    """Graph state constants (see :attr:`Graph.state`)."""

    CREATED = 0
    INITIALIZED = 1
    RUNNING = 2
    DRAINING = 3
    TERMINATED = 4


class DotView:
    """Graphviz detail levels accepted by :meth:`Graph.to_dot`."""

    TOPOLOGY = "topology"
    COMPACT = "compact"
    DIAGNOSTICS = "diagnostics"


# ---------------------------------------------------------------- 算子


class Kernel:
    """Base class for Python kernels.

    Subclasses implement ``process`` (required), optionally ``open`` / ``close``,
    and optionally a static ``get_contract(c)`` to declare port types and required
    side packets.

    Lifecycle is engine-managed: one instance per graph node; ``open`` runs once at
    graph start, ``process`` once per packet (or per aligned timestamp), ``close``
    once at input-close.

    Exceptions raised in these methods are caught, turned into a kernel failure, and
    folded into the graph-level error — they never cross the FFI boundary and crash.
    """

    def open(self, cc: Context) -> None:
        """Called once at graph start. Read options and acquire resources here."""

    def process(self, cc: Context) -> None:
        """Called once per packet. Must be implemented."""
        raise NotImplementedError(
            f"{type(self).__name__} must implement process(self, cc)"
        )

    def close(self, cc: Context) -> None:
        """Called once at input-close. Emit summary results here — but check ``cc.close_reason`` first."""


def kernel(name: str) -> Callable[[type], type]:
    """Register a :class:`Kernel` subclass so YAML can reference it by ``name``.

    Registration happens **when the decorator runs** (i.e. at import time), so make
    sure the import completes before :meth:`Graph.from_yaml`. Registering a duplicate
    name raises.
    """

    def decorator(cls: type) -> type:
        if not issubclass(cls, Kernel):
            raise TypeError(f"{cls.__name__} must subclass lmflow.Kernel")
        _native.register_kernel(name, cls)
        return cls

    return decorator


def registered_kernels() -> Sequence[str]:
    """Names of registered kernels (including the C++ builtins)."""
    return _native.registered_kernels()


def register_builtin_kernels() -> None:
    """Register the bundled C++ builtin kernels (idempotent). Call before building a graph."""
    _native.register_builtin_kernels()


def has_cv_test_kernels() -> bool:
    """Whether the extension bundles the CV test kernel (built with ``python build.py --with-cv-test``).

    The production extension ships **without** OpenCV (ADR #14), so this is False by
    default; only a switch-built extension exposes ``CvInvertTest`` for Python.
    """
    return hasattr(_native, "register_cv_test_kernels")


def register_cv_test_kernels() -> None:
    """Test-only: register the CV kernel ``CvInvertTest``. Available only when the extension was built with ``--with-cv-test``."""
    fn = getattr(_native, "register_cv_test_kernels", None)
    if fn is None:
        raise RuntimeError("extension was not built with CV test kernels. Use: python python/build.py --with-cv-test")
    fn()


def set_log_callback(fn: Callable[[int, str], Any] | None) -> None:
    """Set the log callback ``fn(level, message)``; pass ``None`` to go silent.

    The callback may run on any worker thread (the engine holds no internal lock
    while calling it).
    """
    _native.set_log_callback(fn)


def type_name(type_id: int) -> str:
    """Human-readable name of a type_id (for diagnostics)."""
    return _native.type_name(type_id)


# ---------------------------------------------------------------- 图


class Graph:
    """A computation graph.

    **Always use it as a context manager**: the graph must be stopped before the
    interpreter is destroyed (see the module docstring).
    """

    def __init__(self) -> None:
        self._g = _native.Graph()
        self._closed = False

    # ---- 构造 ----

    @classmethod
    def from_yaml(cls, text: str) -> Graph:
        """Build a graph from YAML text."""
        g = cls()
        g._g.init_from_yaml(text)
        return g

    @classmethod
    def from_yaml_file(cls, path: str | os.PathLike[str]) -> Graph:
        """Build a graph from a YAML file."""
        g = cls()
        g._g.init_from_yaml_file(os.fspath(path))
        return g

    # ---- 上下文管理 ----

    def __enter__(self) -> Graph:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def close(self) -> None:
        """Stop the graph and release resources. Idempotent."""
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
        """Inject a constant input (model handle, calibration params…). Must be before :meth:`start`."""
        self._g.set_side_packet(name, value)

    def add_poller(self, port: str) -> Poller:
        """Pull-mode subscription to a graph output port. Must be before :meth:`start`."""
        return self._g.add_poller(port)

    def observe(self, port: str, fn: Callable[[Packet], Any]) -> None:
        """Push-mode subscription. The callback runs on **the thread that dispatched the packet**; the packet is borrowed and invalid after the callback returns."""
        self._g.observe(port, fn)

    def start(self) -> None:
        """Start the graph and begin scheduling. After this you cannot add_poller / set_side_packet / observe."""
        self._g.start()

    def reset(self) -> None:
        """Reset a finished graph so it can be started again, **keeping already-opened
        kernel instances** — avoids rebuilding the graph and re-running ``open`` (e.g.
        reloading a model) for every session.

        The graph must be Terminated and idle (call :meth:`wait_done` first), else raises.
        Queues / stats / timestamp state are cleared; injected side packets and registered
        pollers/observers are kept, so you can reuse the same poller for the next run.
        """
        self._g.reset()

    # ---- 运行时 ----

    def input(self, port: str) -> Input:
        """Get an input-port handle (avoids per-packet name lookup on the hot path)."""
        return self._g.input(port)

    def send(self, port: str, value: Any, ts: int | None = None) -> None:
        """Convenience send (looks the port up internally). For high frequency, get a handle via :meth:`input`."""
        self._g.input(port).send(value, ts)

    def new_buffer(self, shape: Sequence[int], dtype: Any) -> tuple[Packet, Any]:
        """Have the **engine** allocate a buffer; returns ``(packet, writable numpy view)``.

        This is the recommended zero-copy entry point: write results straight into
        engine memory, avoiding the whole-frame copy of ``send(ndarray)`` and avoiding
        the engine holding a PyObject (see the module docstring's GIL note).
        """
        return self._g.new_buffer(list(shape), dtype)

    def close_input(self, port: str) -> None:
        """Close one input port (tell upstream there is no more data on it)."""
        self._g.close_input(port)

    def close_all_inputs(self) -> None:
        """Close all input ports — lets the graph drain, finish, and terminate."""
        self._g.close_all_inputs()

    def cancel(self) -> None:
        """Cancel immediately: stop scheduling, drop in-flight packets. Does **not** interrupt a kernel callback already running."""
        self._g.cancel()

    def pause(self) -> None:
        """Pause scheduling: in-flight packets stop dispatching; ``send`` can still enqueue."""
        self._g.pause()

    def resume(self) -> None:
        """Resume scheduling paused by :meth:`pause`."""
        self._g.resume()

    def wait_done(self, timeout: float | None = None) -> None:
        """Wait for the graph to finish (close the inputs first). Releases the GIL while waiting."""
        try:
            self._g.wait_done(timeout)
        except TypeError as e:  # 原生层用 TypeError 表示超时
            raise Timeout(str(e)) from None

    def wait_until_idle(self, timeout: float | None = None) -> None:
        """Wait until in-flight packets are all processed, but **do not end the graph** (batch mode)."""
        try:
            self._g.wait_until_idle(timeout)
        except TypeError as e:
            raise Timeout(str(e)) from None

    def pump_step(self) -> bool:
        """Run at most one delegated task on the calling host thread.

        Event-loop hosts can call this repeatedly to advance ``DelegatingExecutor``
        nodes without entering a blocking wait.
        """
        return bool(self._g.pump_step())

    # ---- 内省 ----

    @property
    def state(self) -> int:
        """Current graph state (values in :class:`GraphState`)."""
        return self._g.state

    def dump(self) -> str:
        """Human-readable snapshot of topology and state (node table shows running/elapsed — handy for locating a stall)."""
        return self._g.dump()

    def to_dot(self, view: str = DotView.TOPOLOGY) -> str:
        """Graphviz DOT of the topology (pipe to ``dot -Tsvg``).

        Subgraph namespaces are restored as nested clusters; each node is
        coloured by the thread pool it runs on, and a legend lists every
        executor's thread count, pinned CPU cores (affinity), and realtime
        priority.

        ``view="compact"`` adds node state plus core throughput/latency counters
        without per-port and Poller diagnostics. ``view="diagnostics"`` adds the
        full queue/backpressure detail. Node state uses the border colour while
        the fill remains the latency heat map.
        """
        views = {
            DotView.TOPOLOGY: 0,
            DotView.COMPACT: 1,
            DotView.DIAGNOSTICS: 2,
        }
        try:
            native_view = views[view]
        except KeyError:
            raise ValueError(
                "view must be 'topology', 'compact', or 'diagnostics'"
            ) from None
        return self._g.to_dot_view(native_view)

    def last_error(self) -> str:
        """Graph-level error text — the only place to get a worker-thread kernel's failure reason."""
        return self._g.last_error()

    def queue_depth(self, port: str) -> int:
        """Number of packets currently queued on that input port."""
        return self._g.queue_depth(port)

    def dropped_count(self, port: str) -> int:
        """Cumulative packets dropped on that edge (only the fixed_size policy drops)."""
        return self._g.dropped_count(port)

    def counter_value(self, name: str) -> int:
        """Current value of a kernel-reported counter."""
        return self._g.counter_value(name)

    def total_queued(self) -> int:
        """Total in-flight (enqueued but unconsumed) packets across the graph."""
        return self._g.total_queued()

    def node_names(self) -> list[str]:
        """All node names (in declaration order)."""
        return self._g.node_names()

    def node_stats(self, index: int) -> dict[str, Any]:
        """Node stats: running / running_for_us / processed / errors / elapsed / queued."""
        return self._g.node_stats(index)

    def __repr__(self) -> str:
        names = "?" if self._closed else ",".join(self.node_names())
        return f"<lmflow.Graph nodes=[{names}] state={self.state}>"


def _poller_iter(self: Poller) -> Iterator[Packet]:
    """Let a poller be iterated directly with ``for``: it stops when the graph ends."""
    while True:
        pkt = self.next()
        if pkt is None:
            return
        yield pkt


Poller.__iter__ = _poller_iter  # type: ignore[attr-defined]

# 版本信息
ABI_VERSION = _native.ABI_VERSION
__version__ = "0.3.0"

if sys.version_info < (3, 8):  # pragma: no cover
    raise RuntimeError("lmflow requires Python 3.8+")
