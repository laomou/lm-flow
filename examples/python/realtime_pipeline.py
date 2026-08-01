#!/usr/bin/env python3
"""一个像样的实时管线示例 —— 不是 hello world,而是把框架的关键能力串到一起:

    input(numpy 信号帧)
      └─► normalize   Python 算子,×gain;跑在**线程池**上、**fixed_size** 有界(实时丢旧帧)
            └─► split                     内置 SplitKernel:一进两出(扇出,零拷贝共享)
                  ├─► energy  Python 算子:每帧能量(平方和)──► poller(逐帧取结果)
                  └─►(observe)             推模式:回调里记帧数与峰值

演示点:线程池执行器 · fixed_size 背压 · 扇出 · numpy 零拷贝 · 自定义 Python 算子 ·
        poller(拉)+ observer(推)· 优雅关流。

跑:  PYTHONPATH=.pydeps:python python3 examples/python/realtime_pipeline.py
"""

from __future__ import annotations

import numpy as np

import lmflow

lmflow.register_builtin_kernels()  # 需要内置的 SplitKernel


@lmflow.kernel("Normalize")
class Normalize(lmflow.Kernel):
    """按 gain 缩放整帧。就地写进引擎分配的新缓冲 —— 零拷贝产出。"""

    def open(self, cc):
        self.gain = cc.option_float("gain", 1.0)

    def process(self, cc):
        src = cc.input(0).as_numpy()  # 只读视图
        pkt, dst = cc.new_buffer(src.shape, src.dtype)  # 引擎分配可写缓冲
        np.multiply(src, self.gain, out=dst)
        cc.emit(0, pkt)


@lmflow.kernel("Energy")
class Energy(lmflow.Kernel):
    """每帧输出一个标量:能量(平方和,取整)。演示 buffer→标量 的跨类型产出。"""

    def process(self, cc):
        x = cc.input(0).as_numpy().astype(np.float64)
        cc.emit(0, int(np.sum(x * x)))


PIPELINE = """
executors:
  - { name: cpu, type: ThreadPoolExecutor, num_threads: 2 }
nodes:
  - name: normalize
    kernel: Normalize
    executor: cpu
    input_ports: [in]
    output_ports: [mid]
    options: { gain: 2.0 }
    input_policy: { type: fixed_size, capacity: 4 }   # 实时:处理不过来就丢最旧帧
  - name: split
    kernel: SplitKernel
    input_ports: [mid]
    output_ports: [a, b]                              # 扇出:两条分支各得一份(共享,不拷贝)
  - name: energy
    kernel: Energy
    input_ports: [a]
    output_ports: [out]
input_ports: [in]
output_ports: [out, b]                                # out=能量(poller);b=原始帧(observer)
"""


def main() -> None:
    stats = {"frames": 0, "peak": 0.0}

    with lmflow.Graph.from_yaml(PIPELINE) as g:
        energy_out = g.add_poller("out")
        # 推模式:每帧原始(归一化后)数据都回调一次,这里统计帧数与峰值
        def on_frame(pkt: lmflow.Packet) -> None:
            arr = pkt.as_numpy()
            stats["frames"] += 1
            stats["peak"] = max(stats["peak"], float(arr.max()))

        g.observe("b", on_frame)
        g.start()

        inp = g.input("in")
        # 送 10 帧:每帧是一段递增信号 [i, i+1, i+2, i+3]
        for i in range(10):
            frame = np.arange(i, i + 4, dtype=np.float32)
            inp.send(frame, ts=i)

        g.close_all_inputs()
        g.wait_done(timeout=30.0)

        energies = [p.as_int() for p in energy_out]
        dropped = g.dropped_count("in") or 0
        print(f"送入 10 帧;fixed_size(cap 4)在积压时丢了 {dropped} 帧,处理了 {len(energies)} 帧")
        print(f"逐帧能量(gain=2 后的平方和):{energies}")
        print(f"observer 统计:共 {stats['frames']} 帧,归一化后峰值 {stats['peak']:.1f}")

    # 校验对**丢帧鲁棒**(实时管线丢帧是特性,不是 bug):
    # 第 i 帧 = [i,i+1,i+2,i+3]×2,能量 = Σ(2x)² = 4·Σx²
    valid = {int(4 * sum(v * v for v in range(i, i + 4))) for i in range(10)}
    assert all(e in valid for e in energies), f"每个能量都应是某帧的真实能量:{energies}"
    assert energies == sorted(energies), "应按时间戳升序处理"
    assert stats["frames"] == len(energies), "扇出两分支应看到相同的(丢帧后)帧集"
    assert len(energies) + dropped == 10, "处理 + 丢弃 = 送入(不漏不重)"
    print("OK:实时管线端到端跑通(线程池 + fixed_size 丢帧 + 扇出 + poller/observer)")


if __name__ == "__main__":
    main()
