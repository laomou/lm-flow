/*
 * bridge_smoke.c —— iOS 工具链下的「编译 + 链接」冒烟。
 *
 * flow.h 是纯 C ABI,Swift/Obj-C 都能直接调(见 Demo.swift + module.modulemap)。
 * 本文件只为 CI:用 iOS SDK 编译它、并链接交叉编好的 liblmflow.a,证明
 * 「头文件在 iOS 下能解析 + 符号能解析 + C++ 运行时能链」。CI 只编+链不跑(无真机)。
 */
#include "lmflow/flow.h"

int main(void) {
  if (lmflow_abi_version() != LMFLOW_ABI_VERSION) return 1;
  lmflow_register_builtin_kernels();
  LMFlowGraph* g = lmflow_graph_new();
  if (!g) return 2;
  lmflow_graph_free(g);
  return 0;
}
