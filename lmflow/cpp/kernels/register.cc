/*
 * register.cc —— 内置算子的显式聚合注册。
 *
 * 一文件一算子(见同目录各 .cc);本文件把它们逐个登记进注册表。
 *
 *  算子              用途                          覆盖的接口                       文件
 *  ----------------  ----------------------------  -------------------------------  --------------
 *  PassThrough       零拷贝直通                    Forward                          passthrough.cc
 *  Scale             参数化数值变换                options(OptionI64)、类型声明     scale.cc
 *  Sum               有状态累加,Close 时输出总和  跨包状态、Open/Close、PostStream  sum.cc
 *  Split             1 进 2 出(扇出)             多输出、Forward 到多个口          split.cc
 *  Zip               2 进 1 出(按 tag 取端口)     多输入、InputId(tag)、类型混合    zip.cc
 *  Filter            条件过滤(不产出即推进边界)  不 Emit + SetNextTimestampBound   filter.cc
 *  Stringify         类型转换 int -> std::string   异类型输入输出                    stringify.cc
 *  Sink              只消费不产出                  零输出口                          sink.cc
 *  Invert            原地改写(省拷贝)            TakeInput + CoW MakeMutableBuffer invert.cc
 *  Normalize         参数化归一化                  必需参数/数组/点号路径/side packet normalize.cc
 *  Mux               多路选择                      读控制值转发选中数据口            mux.cc
 *  RangeSource       源(0 输入)产 0..count        SourceDone、生成型算子            range_source.cc
 *  FeedbackAdd       反馈相加 out=正向+反馈(或0)  back-edge(最新值反馈寄存器)      feedback_add.cc
 *  BatchSum          每批求和(batch 策略)         input_policy batch、InputCount/At  batch_sum.cc
 *  ---- 张量预处理(BUFFER,纯数值)----
 *  Cast              dtype 转换(u8→f32 等)         BUFFER 读写 + NewBuffer            cast.cc
 *  Affine            逐元素 x*scale+shift           参数化 BUFFER 变换                 affine.cc
 *  Clamp             逐元素 clamp(x,min,max)         BUFFER 就地阈值                    clamp.cc
 *  Reduce            全缓冲归约 sum/mean/min/max     BUFFER → F64 标量                  reduce.cc
 *
 * 由 Rust 侧 C ABI 包装 lmflow_register_builtin_kernels(见 core/src/lib.rs)
 * 调用一次。用**显式聚合**而非静态初始化:静态库会裁剪未被引用的静态初始化对象
 * (见 docs/design.md §5.1 与 §14 风险登记),
 * 而本函数被导出根引用,链接器不会裁掉它及其引用的各 Register 函数。
 */
#include "builtins.hpp"

extern "C" void lmflow_register_builtin_kernels_impl(void) {
  RegisterPassThroughKernel();
  RegisterScaleKernel();
  RegisterSumKernel();
  RegisterSplitKernel();
  RegisterZipKernel();
  RegisterFilterKernel();
  RegisterStringifyKernel();
  RegisterSinkKernel();
  RegisterInvertKernel();
  RegisterNormalizeKernel();
  RegisterMuxKernel();
  RegisterRangeSourceKernel();
  RegisterFeedbackAddKernel();
  RegisterBatchSumKernel();
  RegisterCastKernel();
  RegisterAffineKernel();
  RegisterClampKernel();
  RegisterReduceKernel();
}
