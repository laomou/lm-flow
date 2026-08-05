#!/usr/bin/env python3
"""OpenCV 管线示例 —— 演示**零拷贝**的正确写法。

拓扑:frames ─► resize(Python + cv2) ─► blur(Python,CoW 原地) ─► invert(C++ 算子) ─► out

关键点(容易写错的地方):
  1. 不要 `send(cv2.imread(...))`。那样要么整帧拷贝,要么引擎持有 PyObject 引用 ——
     后者意味着引用归零可能发生在引擎工作线程上,那里 Py_DECREF 需要抢 GIL,是死锁隐患。
  2. 正确做法:让**引擎分配缓冲**(`new_buffer`),拿到一个零拷贝的 numpy view,
     用 cv2 的 `dst=` 参数把结果**直接写进引擎内存**。全程无跨语言引用计数。
  3. 想就地改写收到的包,先 `take_input()` 再 `make_mutable()`(CoW):
     独占时零拷贝;若上游是扇出、数据被别的分支共享,才会复制一份。
"""

import numpy as np

import lmflow

try:
    import cv2
except ImportError:  # pragma: no cover
    raise SystemExit("this example needs opencv-python: pip install opencv-python") from None

lmflow.register_builtin_kernels()


@lmflow.kernel("PyResizeKernel")
class PyResizeKernel(lmflow.Kernel):
    """把输入图缩放到 options 指定的尺寸。"""

    @staticmethod
    def get_contract(c):
        c.input_set_any(0)
        c.output_set_any(0)

    def open(self, cc):
        self.width = cc.option_int("width", 320)
        self.height = cc.option_int("height", 240)

    def process(self, cc):
        src = cc.input(0).as_numpy()                    # 零拷贝只读视图
        # 让引擎分配输出缓冲,dst 是指向引擎内存的零拷贝 numpy view
        packet, dst = cc.new_buffer((self.height, self.width, 3), np.uint8)
        cv2.resize(src, (self.width, self.height), dst=dst)   # 直接写进引擎内存
        cc.emit(0, packet)


@lmflow.kernel("PyBlurInPlaceKernel")
class PyBlurInPlaceKernel(lmflow.Kernel):
    """就地模糊 —— 演示 CoW 省拷贝路径。"""

    @staticmethod
    def get_contract(c):
        c.input_set_any(0)
        c.output_set_any(0)

    def process(self, cc):
        packet = cc.take_input(0)          # ← 先取走!否则上下文仍持引用,CoW 必然复制
        img = packet.make_mutable()        # 独占 → 零拷贝可写 numpy view
        cv2.GaussianBlur(img, (5, 5), 0, dst=img)   # 原地
        cc.emit(0, packet)


# 两个 Python 算子(resize / blur)挂**委托执行器** —— 它不拥有线程,把节点交还
# Python 主线程跑,于是**没有 GIL 争抢**。invert 是 C++ 算子,放进线程池,可与主线程上的
# Python 算子真正并行。
#
# 注意:不写 executor 会归默认执行器,而默认执行器是线程池 —— Python 算子在那上面要抢 GIL。
# 所以这里必须显式指名 host。
CONFIG = """
executors:
  - name: "host"
    type: "DelegatingExecutor"      # 不拥有线程,交还 Python 主线程 → 无 GIL 争抢
  - name: "cpu"
    type: "ThreadPoolExecutor"
    num_threads: 4
nodes:
  - name: "resize"
    kernel: "PyResizeKernel"
    executor: "host"                # Python 算子留在主线程
    input_ports: ["frames"]
    output_ports: ["small"]
    options: { width: 320, height: 240 }
  - name: "blur"
    kernel: "PyBlurInPlaceKernel"   # Python 算子:take_input + make_mutable 的 CoW 原地改写
    executor: "host"
    input_ports: ["small"]
    output_ports: ["blurred"]
  - name: "invert"
    kernel: "InvertKernel"          # C++ 内置算子,走 CoW 原地改写
    executor: "cpu"                 # 显式 opt-in 并发
    input_ports: ["blurred"]
    output_ports: ["out"]
input_ports: ["frames"]
output_ports: ["out"]
"""


def main() -> None:
    cap = cv2.VideoCapture(0)
    if not cap.isOpened():
        raise SystemExit("cannot open the camera")

    with lmflow.Graph.from_yaml(CONFIG) as graph:
        poller = graph.add_poller("out")
        graph.start()
        frames = graph.input("frames")

        for ts in range(100):
            ok, frame = cap.read()
            if not ok:
                break

            # 引擎分配 + 就地填充:避免把 frame 这个 PyObject 交给引擎
            packet, dst = graph.new_buffer(frame.shape, frame.dtype)
            np.copyto(dst, frame)          # 唯一一次拷贝,发生在受控的入口
            frames.send(packet, ts=ts)

            out = poller.next(timeout=5.0)
            if out is None:
                break
            cv2.imshow("out", out.as_numpy())
            if cv2.waitKey(1) == 27:       # ESC
                break

        graph.close_all_inputs()
        graph.wait_done(timeout=5.0)

    cap.release()
    cv2.destroyAllWindows()


if __name__ == "__main__":
    main()
