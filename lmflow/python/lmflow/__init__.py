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
``send``), explicitly calls ``pump_step``, or runs ``await graph.run_async()``. The async path
uses an engine wakeup callback and ``asyncio.call_soon_threadsafe``; it does not poll.

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

Passing an ndarray to ``send`` or ``Packet.from_numpy`` is zero-copy. The supplied
array is marked read-only while any Packet reference retains it, then its original
writeability is restored. Do not mutate the same allocation through another alias
while the graph owns it. Kernels that request a writable packet view use copy-on-write,
so Python-owned input storage is never modified by the engine.

**Kernel-side API** — ``lmflow/__init__.pyi`` declares the methods available inside
``process(cc)``. Notably, ``cc.side_packet(name)`` reads host-injected constants, while
``cc.input_timestamp`` and ``cc.input(i).timestamp`` expose timestamps for correlating
multiple in-flight data units. The default ``sync`` policy aligns all inputs; use
``sync_set`` only when independent groups must not wait for each other.
"""

from __future__ import annotations

import os
import sys
import asyncio
import warnings
from dataclasses import dataclass
from typing import Any, AsyncIterator, Callable, Iterator, Sequence

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
    "KernelRunner",
    "Packet",
    "Context",
    "Contract",
    "Input",
    "Poller",
    "OutputEvent",
    "PacketEvent",
    "TimestampBoundEvent",
    "DoneEvent",
    "AsyncOutputEvents",
    "LogLevel",
    "CloseReason",
    "GraphState",
    "DotView",
    "Timeout",
    "KernelError",
    "has_cv_test_kernels",
    "register_cv_test_kernels",
    "type_id",
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
KernelRunner = _native.KernelRunner

TS_UNSET = _native.TS_UNSET
TS_PRE_STREAM = _native.TS_PRE_STREAM
TS_POST_STREAM = _native.TS_POST_STREAM
TS_DONE = _native.TS_DONE
INVALID_ID = _native.INVALID_ID


class Timeout(TimeoutError):
    """Raised by timeout-bearing calls when they time out."""


#: Raised when the graph failed to execute — a kernel raised, returned a failure status,
#: emitted a packet of the wrong type, or the graph could not make progress.
#:
#: Subclasses :class:`RuntimeError`, so code that already caught ``RuntimeError`` keeps
#: working; catch this instead to tell an execution failure apart from a cancellation or a
#: bad-state error, which both remain plain :class:`RuntimeError`.
#:
#: Note the engine reports graph *stalls* with the same status code as kernel failures, so a
#: deadlocked or unsatisfiable graph also surfaces here. Read ``str(exc)`` to tell them apart:
#: a genuine kernel failure reads ``kernel failed: [node] ...``, whereas a stall reads
#: ``wait_done: ...`` / ``wait_until_idle: ...``.
KernelError = _native.KernelError


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


class OutputEvent:
    """Base class for typed events yielded by :meth:`Graph.events`."""


@dataclass(frozen=True)
class PacketEvent(OutputEvent):
    """A graph output packet."""

    packet: Packet

    @property
    def timestamp(self) -> int:
        return self.packet.timestamp


@dataclass(frozen=True)
class TimestampBoundEvent(OutputEvent):
    """No later packet on this output can have a timestamp below ``timestamp``."""

    timestamp: int


@dataclass(frozen=True)
class DoneEvent(OutputEvent):
    """The output stream has reached its final timestamp bound."""

    timestamp: int = TS_DONE


class _AsyncGraphDriver:
    """Own the graph-global native wakeup slot and broadcast progress to async waiters."""

    def __init__(self, graph: Graph) -> None:
        self._graph = graph
        self._loop: asyncio.AbstractEventLoop | None = None
        self._wakeup: asyncio.Event | None = None
        self._changed = asyncio.Event()
        self._generation = 0
        self._task: asyncio.Task[None] | None = None

    def _publish(self) -> None:
        changed = self._changed
        self._generation += 1
        self._changed = asyncio.Event()
        changed.set()

    def ensure_running(self) -> None:
        loop = asyncio.get_running_loop()
        if self._task is not None and not self._task.done():
            if self._loop is not loop:
                raise RuntimeError("a graph cannot be driven by two asyncio event loops")
            return

        state = self._graph.state
        if state not in (
            GraphState.INITIALIZED,
            GraphState.RUNNING,
            GraphState.DRAINING,
            GraphState.TERMINATED,
        ):
            raise RuntimeError(
                "async graph APIs require an initialized, running, draining, or terminated graph"
            )

        self._loop = loop
        self._wakeup = asyncio.Event()
        self._changed = asyncio.Event()

        def notify_loop() -> None:
            try:
                loop.call_soon_threadsafe(self._wakeup.set)
            except RuntimeError:
                pass

        self._graph._g.set_wakeup_callback(notify_loop)
        if state == GraphState.INITIALIZED:
            self._graph.start()
        self._task = asyncio.create_task(self._drive())

    async def _drive(self) -> None:
        wakeup = self._wakeup
        assert wakeup is not None
        try:
            while True:
                wakeup.clear()
                while self._graph.pump_step():
                    pass
                self._publish()
                if self._graph.state == GraphState.TERMINATED:
                    return
                if wakeup.is_set():
                    continue
                await wakeup.wait()
        finally:
            self._publish()
            try:
                self._graph._g.set_wakeup_callback(None)
            except RuntimeError:
                pass

    def close(self) -> None:
        task = self._task
        if task is not None and not task.done():
            loop = self._loop
            if loop is not None and loop.is_running():
                try:
                    loop.call_soon_threadsafe(task.cancel)
                except RuntimeError:
                    pass
            else:
                task.cancel()
        try:
            self._graph._g.set_wakeup_callback(None)
        except RuntimeError:
            pass

    async def wait_for_change(self, generation: int) -> int:
        self.ensure_running()
        while self._generation == generation:
            changed = self._changed
            if self._generation != generation:
                break
            await changed.wait()
        return self._generation

    async def wait_terminated(self) -> None:
        self.ensure_running()
        task = self._task
        assert task is not None
        await asyncio.shield(task)

    @property
    def generation(self) -> int:
        return self._generation


class AsyncOutputEvents(AsyncIterator[OutputEvent]):
    """An asynchronous, typed stream for one graph output port."""

    def __init__(self, graph: Graph, poller: Poller) -> None:
        self._graph = graph
        self._poller = poller
        self._done = False

    def __aiter__(self) -> AsyncOutputEvents:
        return self

    async def __anext__(self) -> OutputEvent:
        if self._done:
            raise StopAsyncIteration
        driver = self._graph._get_async_driver()
        driver.ensure_running()
        generation = driver.generation
        while True:
            packet = self._poller.try_next()
            if packet is not None:
                if not packet.is_empty:
                    return PacketEvent(packet)
                if packet.timestamp == TS_DONE:
                    self._done = True
                    await driver.wait_terminated()
                    self._graph.wait_done()
                    return DoneEvent()
                return TimestampBoundEvent(packet.timestamp)
            if self._graph.state == GraphState.TERMINATED:
                self._done = True
                self._graph.wait_done()
                raise StopAsyncIteration
            generation = await driver.wait_for_change(generation)


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


def type_id(stable_name: str) -> int:
    """Return the canonical custom type id for ``stable_name``."""
    return _native.type_id(stable_name)


# ---------------------------------------------------------------- 图


class Graph:
    """A computation graph.

    **Always use it as a context manager**: the graph must be stopped before the
    interpreter is destroyed (see the module docstring).
    """

    def __init__(self) -> None:
        self._g = _native.Graph()
        self._closed = False
        self._async_driver: _AsyncGraphDriver | None = None

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
            if self._async_driver is not None:
                self._async_driver.close()
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

    def add_poller(self, port: str, *, observe_timestamp_bounds: bool = False) -> Poller:
        """Pull-mode subscription to a graph output port. Must be before :meth:`start`.

        With ``observe_timestamp_bounds=True``, the poller also receives empty packets whose
        timestamp is a monotonically advancing bound: no later data packet can have a timestamp
        below that value. ``TS_DONE`` is the final bound.
        """
        return self._g.add_poller(port, observe_timestamp_bounds)

    def events(self, port: str) -> AsyncOutputEvents:
        """Subscribe to typed asynchronous output events. Must be called before :meth:`start`.

        Iteration starts the graph when needed and yields :class:`PacketEvent`,
        :class:`TimestampBoundEvent`, then :class:`DoneEvent`. It shares the graph's single native
        wakeup callback with :meth:`run_async`, so several ports can be consumed concurrently
        without polling or displacing one another.
        """
        return AsyncOutputEvents(
            self, self.add_poller(port, observe_timestamp_bounds=True)
        )

    def observe(
        self,
        port: str,
        fn: Callable[[Packet], Any],
        *,
        observe_timestamp_bounds: bool = False,
    ) -> None:
        """Push-mode subscription. Must be before :meth:`start`, and **cannot be removed** once
        registered — it lives as long as the graph.

        The callback runs on **the thread that dispatched the packet**, with no buffering in
        between, so a slow callback holds up that worker; and the packet is borrowed, becoming
        invalid the moment the callback returns (deep-copy anything you keep). Do not call graph
        lifecycle methods from inside it.

        Several pollers and observers may share one port — each gets its own reference to the same
        packet, with no payload copy. Prefer :meth:`add_poller` unless the callback is trivial:
        a poller is buffered and participates in backpressure, an observer does neither.
        Set ``observe_timestamp_bounds=True`` to also receive empty-packet bound events.
        """
        self._g.observe(port, fn, observe_timestamp_bounds)

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

        Use this when producing data directly into engine-owned memory. Existing
        ndarrays can instead be passed directly to ``send`` without copying.
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
        """Wait for the graph to finish (close the inputs first). Releases the GIL while waiting.

        Raises :class:`Timeout` on expiry, :class:`KernelError` if the run failed, and plain
        ``RuntimeError`` if the graph was cancelled or is in the wrong state.
        """
        try:
            self._g.wait_done(timeout)
        except TimeoutError as e:
            raise Timeout(str(e)) from None

    def wait_until_idle(self, timeout: float | None = None) -> None:
        """Wait until in-flight packets are all processed, but **do not end the graph** (batch mode)."""
        try:
            self._g.wait_until_idle(timeout)
        except TimeoutError as e:
            raise Timeout(str(e)) from None

    def pump_step(self) -> bool:
        """Run at most one delegated task on the calling host thread.

        Event-loop hosts can call this repeatedly to advance ``DelegatingExecutor``
        nodes without entering a blocking wait.
        """
        return bool(self._g.pump_step())

    def pump_steps(self, max_steps: int) -> int:
        """Run at most ``max_steps`` delegated tasks or close-progress steps."""
        if max_steps < 0:
            raise ValueError("max_steps must be non-negative")
        return int(self._g.pump_steps(max_steps))

    async def run_async(
        self,
        *,
        timeout: float | None = None,
        cancel_grace: float = 0.0,
    ) -> None:
        """Run the graph without blocking the current asyncio event loop.

        If the graph is initialized, this method starts it. Engine threads signal the loop through
        a coalesced wakeup callback; delegated tasks are then drained on the event-loop thread.
        Pool-only graphs also wake the loop on completion or failure, so no polling timer is used.

        The graph must eventually terminate: close its inputs, let a source call
        :meth:`Context.source_done`, or call :meth:`cancel`. Cancelling the asyncio task cancels the
        graph and gives it up to ``cancel_grace`` seconds to terminate before re-raising
        :class:`asyncio.CancelledError`. ``timeout`` applies the same graph cancellation and
        cleanup policy, then raises :class:`Timeout`. A second task cancellation abandons a
        graceful wait immediately. The default ``cancel_grace=0.0`` does not wait; pass a positive
        grace when termination before :meth:`reset` matters. :meth:`close` remains the final
        synchronous cleanup fallback.
        """
        if timeout is not None and timeout < 0:
            raise ValueError("timeout must be non-negative or None")
        if cancel_grace < 0:
            raise ValueError("cancel_grace must be non-negative")
        driver = self._get_async_driver()

        async def cancel_and_wait(reason: str) -> None:
            self.cancel()
            cleanup_coro = driver.wait_terminated()
            try:
                cleanup = asyncio.create_task(cleanup_coro)
            except RuntimeError:
                cleanup_coro.close()
                return
            try:
                await asyncio.wait_for(asyncio.shield(cleanup), timeout=cancel_grace)
            except asyncio.TimeoutError:
                warnings.warn(
                    f"run_async {reason} grace expired before the graph terminated; "
                    "Graph.close() will complete synchronous cleanup",
                    RuntimeWarning,
                    stacklevel=3,
                )
                cleanup.cancel()
                try:
                    await cleanup
                except asyncio.CancelledError:
                    pass
            except asyncio.CancelledError:
                cleanup.cancel()
                try:
                    await cleanup
                except asyncio.CancelledError:
                    pass
                raise

        try:
            if timeout is None:
                await driver.wait_terminated()
            else:
                await asyncio.wait_for(driver.wait_terminated(), timeout=timeout)
            self.wait_done()
        except asyncio.TimeoutError as timed_out:
            await cancel_and_wait("timeout")
            raise Timeout(f"run_async timed out after {timeout} seconds") from timed_out
        except asyncio.CancelledError as cancelled:
            current = asyncio.current_task()
            uncancel = getattr(current, "uncancel", None)
            try:
                await cancel_and_wait("cancellation")
            except asyncio.CancelledError:
                if uncancel is not None:
                    uncancel()
            raise cancelled

    def _get_async_driver(self) -> _AsyncGraphDriver:
        if self._async_driver is None:
            self._async_driver = _AsyncGraphDriver(self)
        return self._async_driver

    # ---- 内省 ----

    @property
    def state(self) -> int:
        """Current graph state (values in :class:`GraphState`)."""
        return self._g.state

    def to_dot(self, view: str = DotView.TOPOLOGY) -> str:
        """Graphviz DOT of the topology (pipe to ``dot -Tsvg``).

        Subgraph namespaces are restored as nested clusters; each node is
        coloured by the thread pool it runs on, and a legend lists every
        executor's thread count, pinned CPU cores (affinity), and realtime
        priority. Compact/diagnostics views also show queued, running,
        peak-queued, completed, total wait/execution time, saturation duration,
        and the main queued nodes. A saturated executor is orange, turning red
        after its ready queue remains non-empty for one second.

        ``view="compact"`` adds node state plus core throughput/latency counters
        without per-port and Poller diagnostics. ``view="diagnostics"`` adds the
        full queue/backpressure detail. Waiting source nodes show
        ``WAITING_SOURCE``, remaining wakeup time, yield count, and whether the
        delay came from ``rate``, ``source_yield``, or both. Node state uses the
        border colour while the fill remains the latency heat map.
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

    def to_chrome_trace(self) -> str:
        """Per-invocation execution trace as Chrome Trace Event Format JSON.

        Open ``chrome://tracing`` (or perfetto) and load the string to see a
        timeline of which node ran when, on which worker thread, and for how
        long — detail the aggregate stats (total/max/percentile) cannot show.

        Requires ``trace_capacity: N`` (a bounded event-ring size, 0 = off) at
        the top level of the graph YAML; enabling it forces stats to ``full``
        (per-call timing is needed). When tracing is off this returns a valid
        empty trace (``{"traceEvents": [], ...}``).
        """
        return self._g.to_chrome_trace()

    def last_error(self) -> str:
        """Graph-level error text — the only place to get a worker-thread kernel's failure reason."""
        return self._g.last_error()

    def dropped_count(self, port: str) -> int:
        """Cumulative packets dropped on that edge (only the fixed_size policy drops)."""
        return self._g.dropped_count(port)

    def counter_value(self, name: str) -> int:
        """Current value of a kernel-reported counter."""
        return self._g.counter_value(name)

    def __repr__(self) -> str:
        state = GraphState.TERMINATED if self._closed else self.state
        return f"<lmflow.Graph state={state}>"


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
__version__ = "0.3.3"

if sys.version_info < (3, 8):  # pragma: no cover
    raise RuntimeError("lmflow requires Python 3.8+")
