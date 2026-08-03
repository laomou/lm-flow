#!/usr/bin/env python3
"""图像前处理管线示例:u8 图 → f32 → 归一化(×1/255)→ clamp(0,1)。

演示内置张量算子 CastKernel / AffineKernel / ClampKernel 串成一条 ML 前处理链
(模型输入常见的第一步)。缓冲用 numpy 造、零拷贝穿过引擎。

    python lmflow/examples/python/preprocess/preprocess.py
"""

import numpy as np

import lmflow

CONFIG = """
nodes:
  - { name: cast,  kernel: CastKernel,   input_ports: [in], output_ports: [f],   options: { dtype: f32 } }
  - { name: norm,  kernel: AffineKernel, input_ports: [f],  output_ports: [n],   options: { scale: 0.00392156862745098 } }
  - { name: clamp, kernel: ClampKernel,  input_ports: [n],  output_ports: [out], options: { min: 0.0, max: 1.0 } }
input_ports: [in]
output_ports: [out]
"""


def main() -> None:
    lmflow.register_builtin_kernels()
    with lmflow.Graph.from_yaml(CONFIG) as g:
        out = g.add_poller("out")
        g.start()
        img = np.array([[0, 128, 255], [64, 192, 32]], dtype=np.uint8)  # 2×3 的「图」
        g.input("in").send(img, ts=0)
        norm = out.next(timeout=5.0).as_numpy()
        print("in  (u8):\n", img)
        print("out (f32, /255, clamp[0,1]):\n", norm)
        g.close_all_inputs()
        g.wait_done(timeout=5.0)


if __name__ == "__main__":
    main()
