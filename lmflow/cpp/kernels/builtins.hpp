/*
 * builtins.hpp —— 内置算子的注册声明。
 *
 * 每个算子在自己的 .cc 里定义 Register<X>Kernel();register.cc 逐个调用它们(显式聚合)。
 * 不用静态初始化自注册 —— 静态库会裁剪未被引用的静态初始化对象(见 docs/design.md
 * ADR #14),而这些 Register 函数被导出根 lmflow_register_builtin_kernels 引用,不会被裁。
 */
#ifndef LMFLOW_CPP_KERNELS_BUILTINS_HPP_
#define LMFLOW_CPP_KERNELS_BUILTINS_HPP_

void RegisterPassThroughKernel();
void RegisterScaleKernel();
void RegisterSumKernel();
void RegisterSplitKernel();
void RegisterZipKernel();
void RegisterFilterKernel();
void RegisterStringifyKernel();
void RegisterSinkKernel();
void RegisterInvertKernel();
void RegisterNormalizeKernel();
void RegisterMuxKernel();
void RegisterRangeSourceKernel();
void RegisterFeedbackAddKernel();
void RegisterBatchSumKernel();

#endif  // LMFLOW_CPP_KERNELS_BUILTINS_HPP_
