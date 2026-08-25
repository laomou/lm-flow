"""`__init__.pyi` 必须跟得上 `__init__.py` —— 否则类型检查器看不见半个 API。

为什么需要这个测试:CI 不跑 mypy/pyright/stubtest,所以 stub 漂移**没有任何东西会报**。
写这个测试时,`__all__` 里 30 项有 15 项在 stub 里根本没声明(整个 `OutputEvent` 事件体系、
四个枚举、五个模块级函数),`Graph` 少 13 个公开成员。漂了这么远都没人发现,正说明靠人记着
同步是不够的。

刻意只用 `ast` 静态比对、不 import 本包:这样即使扩展没编译也能跑,`unittest discover`
在任何机器上都不会因为缺 `_lmflow` 而跳过它。
"""

from __future__ import annotations

import ast
import pathlib
import unittest

_PKG = pathlib.Path(__file__).resolve().parents[1] / "lmflow"
_IMPL = _PKG / "__init__.py"
_STUB = _PKG / "__init__.pyi"


def _parse(path: pathlib.Path) -> ast.Module:
    return ast.parse(path.read_text(encoding="utf-8"), filename=str(path))


def _module_level_names(tree: ast.Module) -> set[str]:
    """模块顶层绑定的名字:类、函数、赋值、带注解的声明。"""
    out: set[str] = set()
    for node in tree.body:
        if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            out.add(node.name)
        elif isinstance(node, ast.Assign):
            out |= {t.id for t in node.targets if isinstance(t, ast.Name)}
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            out.add(node.target.id)
    return out


def _public_members(tree: ast.Module, class_name: str) -> set[str]:
    """某个类的公开成员(方法/属性/带注解字段)。dunder 与 _ 前缀不计,`__init__` 除外。"""
    for node in tree.body:
        if isinstance(node, ast.ClassDef) and node.name == class_name:
            out: set[str] = set()
            for b in node.body:
                if isinstance(b, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    if b.name.startswith("_") and b.name != "__init__":
                        continue
                    out.add(b.name)
                elif isinstance(b, ast.AnnAssign) and isinstance(b.target, ast.Name):
                    if not b.target.id.startswith("_"):
                        out.add(b.target.id)
                elif isinstance(b, ast.Assign):
                    out |= {
                        t.id
                        for t in b.targets
                        if isinstance(t, ast.Name) and not t.id.startswith("_")
                    }
            return out
    return set()


def _dunder_all(tree: ast.Module) -> list[str]:
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "__all__" for t in node.targets
        ):
            return [
                e.value for e in node.value.elts if isinstance(e, ast.Constant)
            ]  # type: ignore[attr-defined]
    raise AssertionError("__init__.py 里找不到 __all__")


class StubCompleteness(unittest.TestCase):
    """stub 与实现的公开面必须一致。"""

    @classmethod
    def setUpClass(cls) -> None:
        cls.impl = _parse(_IMPL)
        cls.stub = _parse(_STUB)

    def test_stub_declares_everything_in_dunder_all(self) -> None:
        """`__all__` 是对外承诺的 API;stub 少一项,用户那边就是一个 attr-defined 报错。"""
        exported = _dunder_all(self.impl)
        declared = _module_level_names(self.stub)
        missing = sorted(n for n in exported if n not in declared)
        self.assertEqual(
            missing,
            [],
            f"__all__ 导出了但 __init__.pyi 没声明: {missing}",
        )

    def test_stub_does_not_invent_names(self) -> None:
        """反向:stub 声明了实现里不存在的东西,就是在承诺一个用了会 AttributeError 的 API。

        原生类(Packet/Context/Input/... 来自 _lmflow)在 .py 里是模块级赋值,同样算已定义,
        故这里用模块级名字集合比对,不看它们的成员。
        """
        impl_names = _module_level_names(self.impl)
        # stub 里的类型别名/协议不必在实现里有同名绑定,故只查 __all__ 之外的顶层类与函数。
        invented = sorted(
            n
            for n in _module_level_names(self.stub)
            if n not in impl_names and not n.startswith("_")
        )
        self.assertEqual(invented, [], f"__init__.pyi 声明了实现里没有的名字: {invented}")

    def test_graph_public_surface_matches(self) -> None:
        """`Graph` 是纯 Python 包装类,两边都能静态看到成员,故可逐一比对。"""
        impl_members = _public_members(self.impl, "Graph")
        stub_members = _public_members(self.stub, "Graph")
        missing = sorted(impl_members - stub_members)
        extra = sorted(stub_members - impl_members)
        self.assertEqual(missing, [], f"Graph 有但 stub 没声明的成员: {missing}")
        self.assertEqual(extra, [], f"stub 声明了 Graph 没有的成员: {extra}")

    def test_event_hierarchy_is_declared(self) -> None:
        """`events()` / `observe()` 的返回与回调类型 —— 少了它们,异步流那套完全没法标注。"""
        for cls in ("OutputEvent", "PacketEvent", "TimestampBoundEvent", "DoneEvent"):
            with self.subTest(cls=cls):
                self.assertIn(
                    cls,
                    _module_level_names(self.stub),
                    f"stub 缺事件类型 {cls}",
                )
        # PacketEvent 得带着 packet 字段,不然拿到事件也取不出包。
        self.assertIn(
            "packet",
            _public_members(self.stub, "PacketEvent"),
            "PacketEvent 应声明 packet 字段",
        )


if __name__ == "__main__":
    unittest.main()
