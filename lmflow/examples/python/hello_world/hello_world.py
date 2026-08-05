#!/usr/bin/env python3
"""hello_world —— Python 版最小示例。

两部分:
  1. 用 Python **注册算子**(@lmflow.kernel 装饰器)
  2. 用 Python **驱动图**(送包 / 取结果 / 关流)

拓扑:in ─► scale(C++ 算子) ─► mid ─► py_offset(Python 算子) ─► out

运行:
    python python/build.py --deps      # 首次:下依赖 + 编引擎与扩展
    PYTHONPATH=.pydeps:python python3 examples/python/hello_world.py
"""

import lmflow

lmflow.register_builtin_kernels()   # 内置 C++ 算子(幂等,必须在建图前)

# ---------------------------------------------------------------- 算子:Python 实现
@lmflow.kernel("PyOffsetKernel")
class PyOffsetKernel(lmflow.Kernel):
    """把输入整数加上 options.offset 后输出。

    注意 GIL:Python 算子的 process 在引擎工作线程上被回调,期间持有 GIL,
    因此多个 Python 算子**无法真并行**。重活请交给 C++ 算子。
    """

    @staticmethod
    def get_contract(c):
        c.input_set_any(0)
        c.output_set_any(0)

    def open(self, cc):
        self.offset = cc.option_int("offset", 0)

    def process(self, cc):
        value = cc.input(0).as_int()          # 内建类型,跨语言稳定
        cc.emit(0, value + self.offset)       # 裸 int 也可,引擎自动打包

    def close(self, cc):
        pass


# 两个节点都未指定 executor → 都归**默认执行器**(按 CPU 核数开线程的线程池)。
# ⚠ 于是 Python 算子会在引擎工作线程上抢 GIL。想要「完全没有 GIL 争抢」,把默认换成
# 委托执行器(交还 Python 主线程),一行即可 —— 见 opencv_pipeline 那个例子:
#   executors:
#     - { name: "", type: "DelegatingExecutor" }
CONFIG = """
nodes:
  - name: "scale"
    kernel: "ScaleKernel"           # C++ 内置算子
    input_ports: ["in"]
    output_ports: ["mid"]
    options: { factor: 10 }
  - name: "offset"
    kernel: "PyOffsetKernel"        # 上面用 Python 注册的算子
    input_ports: ["mid"]
    output_ports: ["out"]
    options: { offset: 1 }
input_ports: ["in"]
output_ports: ["out"]
"""


def main() -> None:
    # with 语句确保图在解释器退出前被销毁 —— 否则引擎工作线程可能在解释器
    # 已开始析构时回调进 Python,直接崩溃。这是 Python 侧的硬约束。
    with lmflow.Graph.from_yaml(CONFIG) as graph:
        poller = graph.add_poller("out")
        graph.start()
        source = graph.input("in")

        for i in range(10):
            source.send(i, ts=i)
            packet = poller.next(timeout=5.0)      # 带超时:图卡住时不会永久挂起
            print(f"out: {packet.as_int()} @ ts={packet.timestamp}")

        graph.close_all_inputs()
        graph.wait_done(timeout=5.0)


if __name__ == "__main__":
    main()
