#!/usr/bin/env python3
"""lmflow 的 Python 测试套件。

不依赖 pytest —— 只用标准库 unittest,这样在任何环境都能直接跑:

    PYTHONPATH=.pydeps:python python3 python/tests/test_lmflow.py

覆盖:Python 算子端到端、numpy 零拷贝、写时复制、GIL(线程池上不死锁)、
混合语言管线、异常不穿越 FFI、生命周期、参数、内省。
"""

from __future__ import annotations

import time
import unittest

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


@lmflow.kernel("TBoom")
class TBoom(lmflow.Kernel):
    def process(self, cc):
        raise ValueError("算子内部故意抛异常")


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

    def test_registered_kernels_includes_both_languages(self):
        names = lmflow.registered_kernels()
        self.assertIn("PassThroughKernel", names, "C++ 内置算子")
        self.assertIn("TDouble", names, "Python 算子")


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
            self.assertEqual(arr.dtype, np.float16, "dtype 必须原样保持 float16")
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
                self.assertTrue(np.array_equal(got, view), f"{name} 非连续数组必须原样拷进引擎")
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
            self.assertFalse(arr.flags.writeable, "as_numpy 必须只读")
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
                "线性管线上就地改写不应发生拷贝",
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
                "应是拷贝,引擎不得持有 ndarray(否则工作线程释放它要抢 GIL)",
            )
            g.close_all_inputs()
            g.wait_done(timeout=5.0)

    def test_rejects_unsupported_dtype(self):
        with graph(one_node("PassThroughKernel")) as g:
            g.start()
            with self.assertRaises(ValueError):
                g.input("in").send(np.array(["a", "b"]), ts=0)


# ---------------------------------------------------------------- GIL / 并发


class TestConcurrency(unittest.TestCase):
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
            self.assertEqual(got, [i * 2 for i in range(50)], "max_in_flight 下仍须按序")

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
            self.assertIsNone(out.try_next(), "暂停期间不应产出")
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
            with self.assertRaises(Exception):
                g.wait_done(timeout=5.0)
            # 异常文本必须能拿到 —— 否则算子失败无从诊断
            self.assertIn("故意抛异常", g.last_error())

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
        self.assertTrue(str(ctx.exception), "必须给出可读原因")

    def test_unsupported_config_is_rejected_not_ignored(self):
        with self.assertRaises(ValueError) as ctx:
            graph("nodes: [ { kernel: PassThroughKernel, max_in_flight: 8 } ]")
        self.assertIn("max_in_flight", str(ctx.exception))

    def test_unknown_kernel_lists_available(self):
        with self.assertRaises(Exception) as ctx:
            graph(one_node("NoSuchKernel"))
        self.assertIn("PassThroughKernel", str(ctx.exception), "报错应列出可用算子")

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
  - { name: p, kernel: TSlow, executor: cpu, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"""
        ) as g:
            g.add_poller("out")
            g.start()
            # 输入口没关,wait_done 不会自行结束 → 必须报错而不是永久挂住
            with self.assertRaises(Exception):
                g.wait_done(timeout=0.2)

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
        self.assertIsNone(poller.try_next(), "已结束的图 poller 安全返回 None")


class TestIntrospection(unittest.TestCase):
    def test_stats_counters_and_dump(self):
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
            self.assertEqual(g.counter_value("closed_normally"), 1, "close 恰好一次且为正常结束")
            st = g.node_stats(0)
            self.assertEqual(st["node_name"], "c")
            self.assertEqual(st["kernel_name"], "TCollect")
            self.assertEqual(st["processed"], 7)
            self.assertEqual(st["errors"], 0)
            self.assertFalse(st["running"])
            self.assertIn("node", g.dump())
            self.assertEqual(g.node_names(), ["c"])

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
            self.assertEqual(g.queue_depth("in"), 2)
            self.assertEqual(g.dropped_count("in"), 8, "丢包必须可观测")
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
