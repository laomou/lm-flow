#!/usr/bin/env python3
"""构建 lmflow 的 Python 扩展模块。

为什么是脚本而不是纯 pip:引擎是 Rust 编的静态库,扩展是 pybind11 编的 C++ 模块 ——
两个生态各管一段。本脚本把这两步串起来,不需要 CMake:

    cargo build --release        # 出 libflow_core.a
    g++ -shared ... bindings.cc libflow_core.a -o lmflow/_lmflow.so

用法:
    python python/build.py                # 构建
    python python/build.py --deps         # 顺带把 pybind11/numpy 下到项目内(不装到系统)
    python python/build.py --debug        # 用 debug 版引擎(带断言)
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import sysconfig
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PYDEPS = ROOT / ".pydeps"


def run(cmd: list[str], **kw: object) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True, **kw)  # type: ignore[arg-type]


def fetch_deps() -> None:
    """把 pybind11 与 numpy 解到项目内的 .pydeps —— 不碰系统或用户站点。

    这样在 PEP 668 管控的发行版上也能装,且完全可弃(删目录即可)。
    """
    PYDEPS.mkdir(exist_ok=True)
    wheels = ROOT / ".pydeps-wheels"
    wheels.mkdir(exist_ok=True)
    run([sys.executable, "-m", "pip", "download", "--no-deps",
         "--dest", str(wheels), "pybind11", "numpy"])
    for whl in wheels.glob("*.whl"):
        with zipfile.ZipFile(whl) as z:
            z.extractall(PYDEPS)
    shutil.rmtree(wheels, ignore_errors=True)
    print(f"依赖已解到 {PYDEPS}(用 PYTHONPATH 引入,或由本脚本自动处理)")


def include_dirs() -> list[str]:
    """找 pybind11 与 numpy 的头文件:优先项目内 .pydeps,其次已安装的包。"""
    dirs: list[str] = []

    local_pb = PYDEPS / "pybind11" / "include"
    local_np = PYDEPS / "numpy" / "_core" / "include"
    if local_pb.is_dir():
        dirs.append(str(local_pb))
    if local_np.is_dir():
        dirs.append(str(local_np))

    if not dirs:
        sys.path.insert(0, str(PYDEPS))
        try:
            import pybind11  # noqa: PLC0415
            import numpy  # noqa: PLC0415

            dirs += [pybind11.get_include(), numpy.get_include()]
        except ImportError:
            raise SystemExit(
                "找不到 pybind11 / numpy 头文件。\n"
                "  先跑:python python/build.py --deps\n"
                "  或者:pip install pybind11 numpy"
            ) from None
    return dirs


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--deps", action="store_true", help="先把 pybind11/numpy 下到项目内")
    ap.add_argument("--debug", action="store_true", help="链接 debug 版引擎")
    args = ap.parse_args()

    if args.deps:
        fetch_deps()

    profile = "debug" if args.debug else "release"
    print(f"== 1/2 构建 Rust 引擎({profile})==")
    cargo = ["cargo", "build"] + ([] if args.debug else ["--release"])
    run(cargo, cwd=ROOT)

    lib = ROOT / "target" / profile / "libflow_core.a"
    if not lib.is_file():
        raise SystemExit(f"找不到引擎静态库: {lib}")

    out = ROOT / "python" / "lmflow" / "_lmflow.so"
    src = ROOT / "python" / "src" / "bindings.cc"

    incs = [sysconfig.get_paths()["include"], str(ROOT / "include")] + include_dirs()
    cxx = os.environ.get("CXX", "g++")
    cmd = [
        cxx, "-O2", "-shared", "-std=c++17", "-fPIC",
        # 只导出扩展模块入口:避免把引擎符号泄进宿主进程、与别的库撞名
        "-fvisibility=hidden",
        *(f"-I{d}" for d in incs),
        str(src), str(lib),
        "-lpthread", "-ldl", "-lm",
        "-o", str(out),
    ]
    print("== 2/2 编译 pybind11 扩展 ==")
    run(cmd, cwd=ROOT)
    print(f"\n完成: {out}")
    print("试跑:")
    print(f"  PYTHONPATH={PYDEPS}:{ROOT / 'python'} python3 examples/python/hello_world.py")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
