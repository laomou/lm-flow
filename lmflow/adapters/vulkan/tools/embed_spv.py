#!/usr/bin/env python3
"""把 glslangValidator 产出的 .spv 内嵌成 C++ 头里的 uint32_t 数组。

adapter 刻意**不在构建期编译 shader** —— 那会把 glslang/shaderc 拖成依赖,而目标平台
(Android/移动端)上未必有。代价是 SPIR-V 必须以数组形式签入仓库,于是需要这个工具:
scale_spv.h 的注释一直引用「tests/ 下同名脚本」,但那个脚本从未存在过,导致每加一个
shader 都得手工转二进制。

用法:
    glslangValidator -V -o /tmp/resize.spv adapters/vulkan/shaders/resize.comp
    python3 adapters/vulkan/tools/embed_spv.py /tmp/resize.spv \\
        adapters/vulkan/shaders/resize_spv.h kResizeSpv LMFLOW_VULKAN_SHADERS_RESIZE_SPV_H_

产出与既有 scale_spv.h 同一形状(inline constexpr 数组,每行 6 个字),便于 diff 比对。
"""
import pathlib
import struct
import sys

WORDS_PER_LINE = 6
SPIRV_MAGIC = 0x07230203


def main() -> int:
    if len(sys.argv) != 5:
        print(__doc__)
        return 2
    spv_path, out_path, symbol, guard = sys.argv[1:5]

    blob = pathlib.Path(spv_path).read_bytes()
    if len(blob) % 4 != 0:
        print(f"{spv_path}: 长度 {len(blob)} 不是 4 的倍数,不是合法 SPIR-V", file=sys.stderr)
        return 1
    words = list(struct.unpack(f"<{len(blob) // 4}I", blob))
    if not words or words[0] != SPIRV_MAGIC:
        # 魔数不对通常意味着字节序反了或者根本不是 .spv —— 早失败比产出坏数组好。
        got = f"0x{words[0]:08x}" if words else "(空)"
        print(f"{spv_path}: SPIR-V 魔数应为 0x{SPIRV_MAGIC:08x},实际 {got}", file=sys.stderr)
        return 1

    source = pathlib.Path(spv_path).name
    lines = [
        f"// 由 {source.replace('.spv', '.comp')} 生成,**勿手改**。重新生成:",
        f"//   glslangValidator -V -o /tmp/{source} <this dir>/{source.replace('.spv', '.comp')}",
        f"//   python3 ../tools/embed_spv.py /tmp/{source} \\",
        f"//       {pathlib.Path(out_path).name} {symbol} {guard}",
        "//",
        "// 内嵌而非构建期编译,是为了让 adapter 与它的测试都**不依赖 shader 编译器**",
        "// —— 目标平台(Android/移动端)上通常没有。",
        f"#ifndef {guard}",
        f"#define {guard}",
        "",
        "#include <cstdint>",
        "",
        f"inline constexpr uint32_t {symbol}[] = {{",
    ]
    for i in range(0, len(words), WORDS_PER_LINE):
        chunk = ", ".join(f"0x{w:08x}" for w in words[i : i + WORDS_PER_LINE])
        lines.append(f"    {chunk},")
    lines += ["};", "", f"#endif  // {guard}", ""]

    pathlib.Path(out_path).write_text("\n".join(lines))
    print(f"{out_path}: {len(words)} 个字 ({len(blob)} 字节) -> {symbol}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
