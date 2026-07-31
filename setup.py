#!/usr/bin/env python3
"""pip 安装入口。

引擎是 Rust 编的静态库,扩展是 pybind11 编的 C++ 模块 —— 两个生态各管一段,
所以自定义 build_ext:先 cargo build,再把 bindings.cc 与静态库链在一起。

    pip install .          # 或 pip install -e .

不想走 pip 的话,直接跑 `python python/build.py` 是等价的。
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from setuptools import Extension, setup
from setuptools.command.build_ext import build_ext

ROOT = Path(__file__).resolve().parent


class CargoThenPybind11(build_ext):
    """先构建 Rust 引擎,再编扩展并链上它。"""

    def build_extension(self, ext: Extension) -> None:
        print("== 构建 Rust 引擎(release)==", flush=True)
        subprocess.run(["cargo", "build", "--release"], cwd=ROOT, check=True)
        lib = ROOT / "target" / "release" / "libflow_core.a"
        if not lib.is_file():
            raise SystemExit(f"找不到引擎静态库: {lib}(请确认已安装 Rust 工具链)")
        ext.extra_objects = [str(lib)]

        import pybind11
        import numpy

        ext.include_dirs += [
            pybind11.get_include(),
            numpy.get_include(),
            str(ROOT / "include"),
        ]
        super().build_extension(ext)


setup(
    cmdclass={"build_ext": CargoThenPybind11},
    ext_modules=[
        Extension(
            "lmflow._lmflow",
            sources=["python/src/bindings.cc"],
            language="c++",
            # 只导出扩展入口:避免引擎符号泄进宿主进程与别的库撞名
            extra_compile_args=["-std=c++17", "-O2", "-fvisibility=hidden"],
            libraries=["pthread", "dl", "m"] if sys.platform != "win32" else [],
        )
    ],
)
