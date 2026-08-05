#!/usr/bin/env python3
"""lmflow 的 Python 测试套件。

不依赖 pytest —— 只用标准库 unittest,这样在任何环境都能直接跑:

    PYTHONPATH=.pydeps:python python3 python/tests/test_lmflow.py

覆盖:Python 算子端到端、numpy 零拷贝、写时复制、GIL(线程池上不死锁)、
混合语言管线、异常不穿越 FFI、生命周期、参数、内省。
"""

from __future__ import annotations

import asyncio
import threading
import time
import unittest
from unittest import mock

import numpy as np

import lmflow

lmflow.register_builtin_kernels()


# ---------------------------------------------------------------- 测试用算子
# 注册是全局且一次性的,故在模块级定义。


@lmflow.kernel("TDouble")
class TDouble(lmflow.Kernel):
    @staticmethod
    def get_contract(c):
        c.input_set_any(0)
        c.output_set_any(0)

    def open(self, cc):
        self.factor = cc.option_int("factor", 2)

    def process(self, cc):
        cc.emit(0, cc.input(0).as_int() * self.factor)


@lmflow.kernel("TScaleImg")
class TScaleImg(lmflow.Kernel):
    def process(self, cc):
        src = cc.input(0).as_numpy()
        pkt, dst = cc.new_buffer(src.shape, src.dtype)
        np.multiply(src, 2, out=dst)
        cc.emit(0, pkt)


@lmflow.kernel("TInvert")
class TInvert(lmflow.Kernel):
    """就地改写(写时复制):先 take_input,否则 CoW 必然复制。"""

    def process(self, cc):
        pkt = cc.take_input(0)
        img = pkt.make_mutable()
        np.subtract(255, img, out=img)
        cc.emit(0, pkt)


@lmflow.kernel("TSlow")
class TSlow(lmflow.Kernel):
    def process(self, cc):
        time.sleep(0.001)
        cc.forward(0, 0)


@lmflow.kernel("TTimeoutSlow")
class TTimeoutSlow(lmflow.Kernel):
    def process(self, cc):
        time.sleep(0.1)
        cc.forward(0, 0)


@lmflow.kernel("TBoom")
class TBoom(lmflow.Kernel):
    def process(self, cc):
        raise ValueError("kernel deliberately raised an exception")


@lmflow.kernel("TNeedsOption")
class TNeedsOption(lmflow.Kernel):
    def open(self, cc):
        self.scale = cc.require_option_float("scale")

    def process(self, cc):
        cc.forward(0, 0)


@lmflow.kernel("TNeedsSidePacket")
class TNeedsSidePacket(lmflow.Kernel):
    @staticmethod
    def get_contract(c):
        c.require_side_packet("model")

    def process(self, cc):
        cc.forward(0, 0)


@lmflow.kernel("TCollect")
class TCollect(lmflow.Kernel):
    """有状态 + 用计数器汇报,兼测 close_reason。"""

    def open(self, cc):
        self.n = 0

    def process(self, cc):
        self.n += 1
        cc.counter_add("collected")

    def close(self, cc):
        if cc.close_reason == lmflow.CloseReason.NORMAL:
            cc.counter_add("closed_normally")


@lmflow.kernel("PyFeedbackAdd")
class PyFeedbackAdd(lmflow.Kernel):
    """反馈相加:out = 正向输入 + 上一拍反馈(back-edge,空按 0)。"""

    def process(self, cc):
        v = cc.input(0).as_int()
        if v is None:
            return  # 无正向输入不产出
        fb = cc.input(1).as_int() or 0  # 反馈可空(首拍)→ 0
        cc.emit(0, v + fb)


@lmflow.kernel("PyBatchSum")
class PyBatchSum(lmflow.Kernel):
    """每批求和(batch 策略):用 input_count / input_at 读整批。"""

    def process(self, cc):
        n = cc.input_count(0)
        cc.emit(0, sum(cc.input_at(0, k).as_int() for k in range(n)))


@lmflow.kernel("TYieldSource")
class TYieldSource(lmflow.Kernel):
    def open(self, cc):
        self.calls = 0

    def process(self, cc):
        if self.calls == 0:
            self.calls += 1
            cc.source_yield(0.02)
        else:
            cc.source_done()


CANCEL_STARTED = threading.Event()
CANCEL_RELEASE = threading.Event()


@lmflow.kernel("TCancelSlow")
class TCancelSlow(lmflow.Kernel):
    def process(self, cc):
        CANCEL_STARTED.set()
        CANCEL_RELEASE.wait(timeout=2.0)
        cc.forward(0, 0)


def graph(yaml: str) -> lmflow.Graph:
    return lmflow.Graph.from_yaml(yaml)


ONE_NODE = """
nodes:
  - {{ name: n, kernel: {kernel}, input_ports: [in], output_ports: [out]{opts} }}
input_ports: [in]
output_ports: [out]
"""


def one_node(kernel: str, opts: str = "") -> str:
    return ONE_NODE.format(kernel=kernel, opts=opts)


# ---------------------------------------------------------------- 基本


class TestPythonKernel(unittest.TestCase):
    def test_end_to_end_with_options(self):
        with graph(one_node("TDouble", ", options: { factor: 3 }")) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            got = []
            for i in range(5):
                inp.send(i, ts=i)
                p = out.next(timeout=5.0)
                got.append((p.as_int(), p.timestamp))
            self.assertEqual(got, [(i * 3, i) for i in range(5)])
            g.close_all_inputs()
            g.wait_done(timeout=5.0)
            self.assertEqual(g.state, lmflow.GraphState.TERMINATED)

    def test_poller_is_iterable(self):
        with graph(one_node("TDouble")) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            for i in range(4):
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)
            self.assertEqual([p.as_int() for p in out], [0, 2, 4, 6])

    def test_builtin_cpp_kernel_and_python_kernel_in_one_graph(self):
        with graph(
            """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - { name: pass, kernel: PassThroughKernel, executor: cpu, input_ports: [in], output_ports: [m] }
  - { name: dbl,  kernel: TDouble,                        input_ports: [m],  output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            for i in range(10):
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=15.0)
            self.assertEqual([p.as_int() for p in out], [i * 2 for i in range(10)])

    def test_source_node_produces_and_terminates(self):
        # 源算子(0 输入,内置 RangeSourceKernel)产 0..count,发完自报完成 → 图自然终止。
        with graph(
            """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - { name: src, kernel: RangeSourceKernel, executor: cpu, input_ports: [], output_ports: [out], options: { count: 5 } }
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            g.wait_done(timeout=15.0)  # 没有输入口可喂,源自产
            self.assertEqual([p.as_int() for p in out], [0, 1, 2, 3, 4])

    def test_run_async_drives_delegating_executor(self):
        async def scenario():
            with graph(
                """
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: dbl, kernel: TDouble, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
            ) as g:
                out = g.add_poller("out")
                run = asyncio.create_task(g.run_async())
                await asyncio.sleep(0)
                g.input("in").send(21, ts=0)
                g.close_all_inputs()
                await asyncio.wait_for(run, timeout=2.0)
                self.assertEqual(out.try_next().as_int(), 42)

        asyncio.run(scenario())

    def test_run_async_waits_for_pool_source_without_polling(self):
        async def scenario():
            with graph(
                """
nodes:
  - { name: src, kernel: RangeSourceKernel, input_ports: [], output_ports: [out],
      options: { count: 3 }, rate: 100.0 }
output_ports: [out]
"""
            ) as g:
                out = g.add_poller("out")
                await asyncio.wait_for(g.run_async(), timeout=2.0)
                self.assertEqual([packet.as_int() for packet in out], [0, 1, 2])

        asyncio.run(scenario())

    def test_run_async_cancellation_waits_briefly_for_running_pool_kernel(self):
        async def scenario():
            with graph(
                """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: slow, kernel: TCancelSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
            ) as g:
                CANCEL_STARTED.clear()
                CANCEL_RELEASE.clear()
                try:
                    run = asyncio.create_task(g.run_async(cancel_grace=1.0))
                    await asyncio.sleep(0)
                    g.input("in").send(1, ts=0)
                    self.assertTrue(await asyncio.to_thread(CANCEL_STARTED.wait, 1.0))
                    run.cancel()
                    await asyncio.sleep(0)
                    self.assertFalse(run.done())
                    CANCEL_RELEASE.set()
                    with self.assertRaises(asyncio.CancelledError):
                        await run
                    self.assertEqual(g.state, lmflow.GraphState.TERMINATED)
                finally:
                    CANCEL_RELEASE.set()

        asyncio.run(scenario())

    def test_run_async_second_cancellation_abandons_graceful_wait(self):
        async def scenario():
            with graph(
                """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: slow, kernel: TCancelSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
            ) as g:
                CANCEL_STARTED.clear()
                CANCEL_RELEASE.clear()
                try:
                    run = asyncio.create_task(g.run_async(cancel_grace=10.0))
                    await asyncio.sleep(0)
                    g.input("in").send(1, ts=0)
                    self.assertTrue(await asyncio.to_thread(CANCEL_STARTED.wait, 1.0))
                    run.cancel()
                    await asyncio.sleep(0)
                    self.assertFalse(run.done())
                    run.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await asyncio.wait_for(run, timeout=0.2)
                    if hasattr(run, "cancelling"):
                        self.assertEqual(run.cancelling(), 1)
                    self.assertNotEqual(g.state, lmflow.GraphState.TERMINATED)
                finally:
                    CANCEL_RELEASE.set()

        asyncio.run(scenario())

    def test_run_async_cancel_grace_has_an_upper_bound(self):
        async def scenario():
            with graph(
                """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: slow, kernel: TCancelSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
            ) as g:
                CANCEL_STARTED.clear()
                CANCEL_RELEASE.clear()
                try:
                    run = asyncio.create_task(g.run_async(cancel_grace=0.01))
                    await asyncio.sleep(0)
                    g.input("in").send(1, ts=0)
                    self.assertTrue(await asyncio.to_thread(CANCEL_STARTED.wait, 1.0))
                    started = time.monotonic()
                    run.cancel()
                    with self.assertWarnsRegex(RuntimeWarning, "grace expired"):
                        with self.assertRaises(asyncio.CancelledError):
                            await asyncio.wait_for(run, timeout=0.2)
                    if hasattr(run, "cancelling"):
                        self.assertEqual(run.cancelling(), 1)
                    self.assertLess(time.monotonic() - started, 0.2)
                    self.assertNotEqual(g.state, lmflow.GraphState.TERMINATED)
                finally:
                    CANCEL_RELEASE.set()

        asyncio.run(scenario())

    def test_run_async_loop_shutdown_preserves_cancelled_error(self):
        async def scenario():
            with graph(
                """
nodes:
  - { name: src, kernel: RangeSourceKernel, input_ports: [], output_ports: [out],
      options: { count: 100 }, rate: 0.1 }
output_ports: [out]
"""
            ) as g:
                run = asyncio.create_task(g.run_async(cancel_grace=1.0))
                await asyncio.sleep(0)
                with mock.patch("asyncio.create_task", side_effect=RuntimeError("loop closed")):
                    run.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await run

        asyncio.run(scenario())

    def test_run_async_cancellation_reaches_terminated_for_delegated_graph(self):
        async def scenario():
            with graph(
                """
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: dbl, kernel: TDouble, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
            ) as g:
                run = asyncio.create_task(g.run_async(cancel_grace=1.0))
                await asyncio.sleep(0)
                g.input("in").send(21, ts=0)
                run.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await run
                self.assertEqual(g.state, lmflow.GraphState.TERMINATED)

        asyncio.run(scenario())

    def test_run_async_cancellation_does_not_wait_by_default(self):
        async def scenario():
            with graph(
                """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: slow, kernel: TCancelSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
            ) as g:
                CANCEL_STARTED.clear()
                CANCEL_RELEASE.clear()
                try:
                    run = asyncio.create_task(g.run_async())
                    await asyncio.sleep(0)
                    g.input("in").send(1, ts=0)
                    self.assertTrue(await asyncio.to_thread(CANCEL_STARTED.wait, 1.0))
                    started = time.monotonic()
                    run.cancel()
                    with self.assertWarnsRegex(RuntimeWarning, "grace expired"):
                        with self.assertRaises(asyncio.CancelledError):
                            await asyncio.wait_for(run, timeout=0.2)
                    self.assertLess(time.monotonic() - started, 0.2)
                    self.assertNotEqual(g.state, lmflow.GraphState.TERMINATED)
                finally:
                    CANCEL_RELEASE.set()

        asyncio.run(scenario())

    def test_source_yield_releases_worker_and_retries(self):
        with graph(
            """
executors:
  - { name: solo, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: src, kernel: TYieldSource, executor: solo, input_ports: [], output_ports: [] }
"""
        ) as g:
            started = time.monotonic()
            g.start()
            g.wait_done(timeout=1.0)
            self.assertGreaterEqual(time.monotonic() - started, 0.015)

    def test_subgraph_expands_and_runs(self):
        # 子图是纯 YAML / 建图期展开,绑定无需改:一个 PassPair 实例展开成两级直通。
        with graph(
            """
subgraphs:
  PassPair:
    nodes:
      - { name: a, kernel: PassThroughKernel, input_ports: [sin], output_ports: [mid] }
      - { name: b, kernel: PassThroughKernel, input_ports: [mid], output_ports: [sout] }
    input_ports: [sin]
    output_ports: [sout]
nodes:
  - { name: p, type: PassPair, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            for i in range(5):
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)
            self.assertEqual([p.as_int() for p in out], [0, 1, 2, 3, 4])
            # 展开后内部节点被命名空间化为 p/a、p/b
            dot = g.to_dot()
            self.assertIn("p/a", dot)
            self.assertIn("p/b", dot)

    def test_back_edge_feedback_loop(self):
        # 反馈自环:out 经 back_edge 回灌;out(t)=in(t)+out(t-1)。输入关闭后正常终止。
        with graph(
            """
nodes:
  - name: acc
    kernel: PyFeedbackAdd
    input_ports: [in, out]
    output_ports: [out]
    back_edges: [out]
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            for i in range(5):
                inp.send(1, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)
            self.assertEqual([p.as_int() for p in out], [1, 2, 3, 4, 5])
            self.assertEqual(g.state, lmflow.GraphState.TERMINATED)

    def test_batch_policy(self):
        # 攒够 capacity 个包一次交给算子;关流时余批刷出。
        with graph(
            """
nodes:
  - name: b
    kernel: PyBatchSum
    input_ports: [in]
    output_ports: [out]
    input_policy: { type: batch, capacity: 3 }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            for i in range(1, 8):  # 1..7
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)
            # 批 [1,2,3]=6、[4,5,6]=15,关流余 [7]=7
            self.assertEqual([p.as_int() for p in out], [6, 15, 7])

    def test_registered_kernels_includes_both_languages(self):
        names = lmflow.registered_kernels()
        self.assertIn("PassThroughKernel", names, "C++ built-in kernel")
        self.assertIn("TDouble", names, "Python kernel")


# ---------------------------------------------------------------- numpy


class TestNumpy(unittest.TestCase):
    def test_zero_copy_roundtrip(self):
        with graph(one_node("TScaleImg")) as g:
            out = g.add_poller("out")
            g.start()
            pkt, buf = g.new_buffer((2, 3), np.uint8)
            buf[:] = [[1, 2, 3], [4, 5, 6]]
            g.input("in").send(pkt, ts=0)
            arr = out.next(timeout=5.0).as_numpy()
            self.assertEqual(arr.tolist(), [[2, 4, 6], [8, 10, 12]])
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_float16_roundtrip(self):
        # fp16 是模型推理主力类型:new_buffer 能建、零拷贝穿过管线、as_numpy 读回都须为 float16
        with graph(one_node("PassThroughKernel")) as g:
            out = g.add_poller("out")
            g.start()
            pkt, buf = g.new_buffer((4,), np.float16)
            self.assertEqual(buf.dtype, np.float16)
            buf[:] = [1.5, 2.0, -3.25, 0.0]
            g.input("in").send(pkt, ts=0)
            arr = out.next(timeout=5.0).as_numpy()
            self.assertEqual(arr.dtype, np.float16, "dtype must be preserved as float16")
            self.assertEqual(arr.tolist(), [1.5, 2.0, -3.25, 0.0])
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_non_contiguous_ndarray_roundtrip(self):
        # 转置/步长切片/负步长的 numpy 视图都是非连续的 —— 拷进引擎必须按 strides 拷对,
        # 否则静默数据损坏(曾经就是:只认倒数第二维的 stride,整行 memcpy)。
        a = np.arange(12, dtype=np.int32).reshape(3, 4)
        cases = {
            "transpose": a.T,
            "slice_step": a[:, ::2],
            "reversed": a[::-1],
            "3d_transpose": np.arange(24, dtype=np.float32).reshape(2, 3, 4).transpose(1, 0, 2),
        }
        for name, view in cases.items():
            with graph(one_node("PassThroughKernel")) as g:
                out = g.add_poller("out")
                g.start()
                g.input("in").send(view, ts=0)
                got = out.next(timeout=5.0).as_numpy()
                self.assertTrue(np.array_equal(got, view), f"{name} non-contiguous array must be copied into the engine as-is")
                g.close_all_inputs()
                g.wait_done(timeout=5.0)

    def test_as_numpy_is_read_only(self):
        # 输入包是引用计数共享的,写它会污染别的分支 —— 必须是只读视图
        with graph(one_node("PassThroughKernel")) as g:
            out = g.add_poller("out")
            g.start()
            pkt, buf = g.new_buffer((4,), np.float32)
            buf[:] = [1, 2, 3, 4]
            g.input("in").send(pkt, ts=0)
            arr = out.next(timeout=5.0).as_numpy()
            self.assertFalse(arr.flags.writeable, "as_numpy must be read-only")
            with self.assertRaises(ValueError):
                arr[0] = 9
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_cow_is_zero_copy_on_linear_pipeline(self):
        # 三级管线:上游 ctx 若残留引用,CoW 会退化成全量拷贝(地址会变)
        with graph(
            """
nodes:
  - { name: a, kernel: PassThroughKernel, input_ports: [in], output_ports: [m1] }
  - { name: b, kernel: PassThroughKernel, input_ports: [m1], output_ports: [m2] }
  - { name: c, kernel: TInvert,           input_ports: [m2], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            pkt, buf = g.new_buffer((4,), np.uint8)
            buf[:] = [0, 1, 2, 3]
            addr = buf.__array_interface__["data"][0]
            g.input("in").send(pkt, ts=0)
            arr = out.next(timeout=5.0).as_numpy()
            self.assertEqual(arr.tolist() , [255, 254, 253, 252])
            self.assertEqual(
                arr.__array_interface__["data"][0], addr,
                "in-place modification on a linear pipeline should not copy",
            )
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_send_ndarray_copies(self):
        # send(ndarray) 会拷贝一份进引擎 —— 这是安全的默认;想省拷贝请用 new_buffer
        with graph(one_node("PassThroughKernel")) as g:
            out = g.add_poller("out")
            g.start()
            src = np.arange(6, dtype=np.int32).reshape(2, 3)
            g.input("in").send(src, ts=0)
            arr = out.next(timeout=5.0).as_numpy()
            self.assertEqual(arr.tolist(), src.tolist())
            self.assertNotEqual(
                arr.__array_interface__["data"][0],
                src.__array_interface__["data"][0],
                "should be a copy; the engine must not hold the ndarray (otherwise the worker thread would need the GIL to free it)",
            )
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_rejects_unsupported_dtype(self):
        with graph(one_node("PassThroughKernel")) as g:
            g.start()
            with self.assertRaises(ValueError):
                g.input("in").send(np.array(["a", "b"]), ts=0)

    def test_buffer_preprocess_pipeline(self):
        # 内置张量前处理链:u8 图 → Cast(f32) → Affine(×1/255) → Clamp(0,1)。
        with graph(
            """
nodes:
  - { name: cast,  kernel: CastKernel,   input_ports: [in], output_ports: [f],   options: { dtype: f32 } }
  - { name: norm,  kernel: AffineKernel, input_ports: [f],  output_ports: [n],   options: { scale: 0.00392156862745098 } }
  - { name: clamp, kernel: ClampKernel,  input_ports: [n],  output_ports: [out], options: { min: 0.0, max: 1.0 } }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            g.input("in").send(np.array([0, 128, 255], dtype=np.uint8), ts=0)
            arr = out.next(timeout=5.0).as_numpy()
            self.assertEqual(arr.dtype, np.float32)
            np.testing.assert_allclose(arr, [0.0, 128 / 255, 1.0], atol=1e-4)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_reduce_mean_emits_scalar(self):
        # 全缓冲归约成 F64 标量。
        with graph(
            """
nodes:
  - { name: r, kernel: ReduceKernel, input_ports: [in], output_ports: [out], options: { op: mean } }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            g.input("in").send(np.array([2, 4, 6], dtype=np.float32), ts=0)
            self.assertAlmostEqual(out.next(timeout=5.0).as_float(), 4.0)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)


# ---------------------------------------------------------------- GIL / 并发


class TestConcurrency(unittest.TestCase):
    def test_delegating_executor_can_be_pumped_explicitly(self):
        with graph(
            """
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: p, kernel: TDouble, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            g.input("in").send(7, ts=0)
            self.assertIsNone(out.try_next())
            self.assertTrue(g.pump_step())
            self.assertEqual(out.try_next().as_int(), 14)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_python_kernel_on_thread_pool_does_not_deadlock(self):
        with graph(
            """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 4 }
nodes:
  - { name: p, kernel: TSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            for i in range(50):
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=30.0)
            self.assertEqual(sum(1 for _ in out), 50)

    def test_observe_callback_runs_and_receives_packets(self):
        seen = []
        with graph(one_node("TDouble")) as g:
            g.observe("out", lambda p: seen.append(p.as_int()))
            g.start()
            inp = g.input("in")
            for i in range(5):
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)
        self.assertEqual(seen, [i * 2 for i in range(5)])

    def test_max_in_flight_preserves_order(self):
        # Python 算子在 max_in_flight>1 的线程池上:GIL 会串行化 process,
        # 但引擎的按序重排路径仍被走到,输出必须按时间戳单调、且不崩。
        with graph(
            """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 4 }
nodes:
  - name: d
    kernel: TDouble
    executor: cpu
    max_in_flight: 4
    input_ports: [in]
    output_ports: [out]
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            for i in range(50):
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=30.0)
            got = [p.as_int() for p in out]
            self.assertEqual(got, [i * 2 for i in range(50)], "must still be in order under max_in_flight")

    def test_pause_and_resume(self):
        with graph(
            """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - { name: p, kernel: TDouble, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            g.pause()
            inp = g.input("in")
            for i in range(5):
                inp.send(i, ts=i)
            time.sleep(0.05)
            self.assertIsNone(out.try_next(), "should not produce output while paused")
            g.resume()
            g.wait_until_idle(timeout=10.0)
            self.assertEqual(sum(1 for _ in iter(out.try_next, None)), 5)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)


# ---------------------------------------------------------------- 错误与生命周期


class TestErrors(unittest.TestCase):
    def test_python_exception_becomes_engine_error(self):
        with graph(one_node("TBoom")) as g:
            g.add_poller("out")
            g.start()
            g.input("in").send(1, ts=0)
            g.close_all_inputs()
            with self.assertRaises(lmflow.KernelError):
                g.wait_done(timeout=5.0)
            # 异常文本必须能拿到 —— 否则算子失败无从诊断
            self.assertIn("deliberately raised an exception", g.last_error())

    def test_kernel_error_still_caught_as_runtime_error(self):
        # KernelError 派生自 RuntimeError,所以本次改动前写的 `except RuntimeError` 不能失效。
        self.assertTrue(issubclass(lmflow.KernelError, RuntimeError))
        with graph(one_node("TBoom")) as g:
            g.start()
            g.input("in").send(1, ts=0)
            g.close_all_inputs()
            with self.assertRaises(RuntimeError):
                g.wait_done(timeout=5.0)

    def test_cancellation_is_not_reported_as_kernel_error(self):
        # 这才是加 KernelError 的目的:从前 cancel 与算子失败都塌成裸 RuntimeError,分不开。
        with graph(one_node("TDouble")) as g:
            g.start()
            g.cancel()
            with self.assertRaises(RuntimeError) as ctx:
                g.wait_done(timeout=5.0)
            self.assertNotIsInstance(ctx.exception, lmflow.KernelError)

    def test_missing_required_option_fails_at_start(self):
        with graph(one_node("TNeedsOption")) as g:
            with self.assertRaises(Exception) as ctx:
                g.start()
            self.assertIn("scale", str(ctx.exception))

    def test_missing_required_side_packet_fails_at_start(self):
        with graph(one_node("TNeedsSidePacket")) as g:
            with self.assertRaises(Exception) as ctx:
                g.start()
            self.assertIn("model", str(ctx.exception))

    def test_side_packet_satisfies_requirement(self):
        with graph(one_node("TNeedsSidePacket")) as g:
            g.set_side_packet("model", b"fake-weights")
            g.add_poller("out")
            g.start()
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_bad_yaml_reports_readable_reason(self):
        with self.assertRaises(Exception) as ctx:
            graph("nodes: [ { kernel: X, typo_field: 1 } ]")
        self.assertTrue(str(ctx.exception), "must provide a readable reason")

    def test_unsupported_config_is_rejected_not_ignored(self):
        # max_in_flight > 1 要求所属执行器的线程数 > 1。默认执行器是按 CPU 核数开的
        # 线程池,所以「不写 executor + max_in_flight: 8」现在是**合法**的 ——
        # 要构造这个错误,得显式挂一个单线程执行器。
        with self.assertRaises(ValueError) as ctx:
            graph(
                """
executors:
  - { name: solo, type: ThreadPoolExecutor, num_threads: 1 }
nodes: [ { kernel: PassThroughKernel, executor: solo, max_in_flight: 8 } ]
"""
            )
        self.assertIn("max_in_flight", str(ctx.exception))

    def test_unknown_kernel_lists_available(self):
        with self.assertRaises(Exception) as ctx:
            graph(one_node("NoSuchKernel"))
        self.assertIn("PassThroughKernel", str(ctx.exception), "error should list available kernels")

    def test_rejects_non_builtin_payload(self):
        with graph(one_node("PassThroughKernel")) as g:
            g.start()
            # 任意 Python 对象不能进数据流(见模块文档的数据类型一节)
            with self.assertRaises(TypeError):
                g.input("in").send({"a": 1}, ts=0)

    def test_timeout_raises(self):
        with graph(
            """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: p, kernel: TTimeoutSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            g.add_poller("out")
            g.start()
            g.input("in").send(1, ts=0)
            g.close_all_inputs()
            with self.assertRaises(lmflow.Timeout):
                g.wait_done(timeout=0.001)

    def test_wait_until_idle_timeout_raises(self):
        with graph(
            """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: p, kernel: TTimeoutSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            g.start()
            g.input("in").send(1, ts=0)
            with self.assertRaises(lmflow.Timeout):
                g.wait_until_idle(timeout=0.001)

    def test_wait_timeout_argument_type_error_is_not_rewritten(self):
        with graph(one_node("TDouble")) as g:
            with self.assertRaises(TypeError):
                g.wait_done(timeout="abc")
            with self.assertRaises(TypeError):
                g.wait_until_idle(timeout="abc")

    def test_close_is_idempotent(self):
        g = graph(one_node("TDouble"))
        g.close()
        g.close()

    def test_handle_use_after_graph_closed_is_safe(self):
        # 句柄由调用方拥有并各持一份对引擎的引用:即使先关掉/销毁了图,
        # 用悬空句柄也只会安全报错,绝不 use-after-free(曾经会挂死)。
        g = graph(one_node("TDouble"))
        poller = g.add_poller("out")
        g.start()
        inp = g.input("in")
        g.close()  # 显式关图;句柄仍在手上
        with self.assertRaises(Exception):
            inp.send(1, ts=0)  # 必须安全报「已关闭」,而不是崩溃/挂死
        self.assertIsNone(poller.try_next(), "poller on a terminated graph safely returns None")


class TestIntrospection(unittest.TestCase):
    def test_counters_and_diagnostics_dot(self):
        with graph(
            """
nodes:
  - { name: c, kernel: TCollect, input_ports: [in], output_ports: [] }
input_ports: [in]
"""
        ) as g:
            g.start()
            inp = g.input("in")
            for i in range(7):
                inp.send(i, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

            self.assertEqual(g.counter_value("collected"), 7)
            self.assertEqual(g.counter_value("closed_normally"), 1, "close called exactly once and with normal termination")
            dot = g.to_dot(lmflow.DotView.DIAGNOSTICS)
            self.assertIn("c", dot)
            self.assertIn("TCollect", dot)
            self.assertIn("7 pkts", dot)
            self.assertIn("CLOSED", dot)

    def test_to_dot_exports_graphviz(self):
        # 纯拓扑快照:digraph 头 + 子图命名空间还原成 cluster + 执行器落位标注。
        with graph(
            """
subgraphs:
  Pair:
    nodes:
      - { name: a, kernel: PassThroughKernel, input_ports: [sin], output_ports: [mid] }
      - { name: b, kernel: PassThroughKernel, input_ports: [mid], output_ports: [sout] }
    input_ports: [sin]
    output_ports: [sout]
nodes:
  - { name: p, type: Pair, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            dot = g.to_dot()
            self.assertIn("digraph lmflow", dot)
            self.assertIn("subgraph cluster_", dot)
            self.assertIn("@default", dot)
            compact = g.to_dot(view=lmflow.DotView.COMPACT)
            self.assertIn("@default\\nCREATED", compact)
            self.assertNotIn("CREATED · 0 pkts", compact)
            self.assertIn("hotspots running 0 · error 0", compact)
            self.assertIn("cluster_node_state_legend", compact)
            self.assertNotIn("ports:", compact)
            diagnostics = g.to_dot(view=lmflow.DotView.DIAGNOSTICS)
            self.assertIn("ports:", diagnostics)
            with self.assertRaises(ValueError):
                g.to_dot(view="verbose")

    def test_fixed_size_policy_drops_and_reports(self):
        with graph(
            """
nodes:
  - name: p
    kernel: PassThroughKernel
    input_ports: [in]
    output_ports: [out]
    input_policy: { type: fixed_size, capacity: 2 }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            inp = g.input("in")
            # 暂停隔离 fixed_size:否则空闲节点会立刻认领第一个包(不受丢弃约束)。
            g.pause()
            for i in range(10):
                inp.send(i, ts=i)
            diagnostics = g.to_dot(lmflow.DotView.DIAGNOSTICS)
            self.assertIn("queue 2/unbounded", diagnostics)
            self.assertIn("dropped +8", diagnostics)
            self.assertEqual(g.dropped_count("in"), 8, "dropped packets must be observable")
            g.resume()
            g.wait_until_idle(timeout=5.0)
            self.assertEqual([p.as_int() for p in iter(out.try_next, None)], [8, 9])
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

class TestTimestampSync(unittest.TestCase):
    def test_multi_input_pairs_by_timestamp(self):
        @lmflow.kernel("TSum2")
        class TSum2(lmflow.Kernel):
            def process(self, cc):
                a = cc.input(0).as_int()
                b = cc.input(1).as_int()
                if a is None or b is None:
                    return  # 该时刻缺一路
                cc.emit(0, a + b)

        with graph(
            """
nodes:
  - { name: z, kernel: TSum2, input_ports: ["A:x", "B:y"], output_ports: [out] }
input_ports: [x, y]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            a, b = g.input("x"), g.input("y")
            for i in range(5):
                a.send(i, ts=i)
                b.send(i * 100, ts=i)
            g.close_all_inputs()
            g.wait_done(timeout=10.0)
            got = [(p.timestamp, p.as_int()) for p in out]
            self.assertEqual(got, [(i, i + i * 100) for i in range(5)])


class TestPolicyExtras(unittest.TestCase):
    def test_sync_set_partial_alignment(self):
        # 分组 {x,y} 与 {z}:各组独立按时间戳对齐、独立触发,每次只带该组的口。
        @lmflow.kernel("Probe3")
        class Probe3(lmflow.Kernel):
            def process(self, cc):
                # 位掩码:bit i = 输入口 i 本次非空
                mask = sum(1 << i for i in range(3) if not cc.input(i).is_empty)
                cc.emit(0, mask)

        with graph(
            """
nodes:
  - { name: n, kernel: Probe3, input_ports: [x, y, z], output_ports: [out],
      input_policy: { type: sync_set, sets: [[x, y], [z]] } }
input_ports: [x, y, z]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            g.input("x").send(1, ts=0)
            g.input("y").send(2, ts=0)  # {x,y} 组齐 → 掩码 0b011
            g.input("z").send(3, ts=1)  # {z} 组 → 掩码 0b100
            g.close_all_inputs()
            g.wait_done(timeout=10.0)
            self.assertEqual([p.as_int() for p in out], [0b011, 0b100])

    def test_mux_kernel_selects_data_port(self):
        # MuxKernel:输入 0=控制(选择器),1..=数据口。默认 sync 全对齐。
        with graph(
            """
nodes:
  - { name: m, kernel: MuxKernel, input_ports: [sel, a, b], output_ports: [out] }
input_ports: [sel, a, b]
output_ports: [out]
"""
        ) as g:
            out = g.add_poller("out")
            g.start()
            for ts, k, av, bv in [(0, 0, 100, 200), (1, 1, 101, 201)]:
                g.input("sel").send(k, ts=ts)
                g.input("a").send(av, ts=ts)
                g.input("b").send(bv, ts=ts)
            g.close_all_inputs()
            g.wait_done(timeout=10.0)
            self.assertEqual([p.as_int() for p in out], [100, 201], "ts0 selects a, ts1 selects b")


if __name__ == "__main__":
    unittest.main(verbosity=2)
