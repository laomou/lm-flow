// bridge.cc —— OpenCL CPU↔GPU 桥接算子的注册(注册名 "OclUpload" / "OclDownload")。
//
// 算子类本体在 <lmflow/opencl.hpp>(UploadKernel / DownloadKernel):它们是纯 plumbing,
// 头文件**故意不自注册** —— 头会被多个 TU 包含,在头里放全局注册对象会到处重复注册。
// 注册的正确落点是随 adapter 发布的这个 kernels archive:一处静态自注册,图里即可直接写
// `kernel: OclUpload` / `kernel: OclDownload`,宿主不必再手写注册。
//
// 有了这两个桥接算子随 archive 落地,一条 CPU→GPU→CPU 的图才是**开箱即用**的:
//   OclUpload → OclResize → OclAffine → … → OclDownload
// 中间的 ocl::Image 一路驻留设备、不落主机(设备 buffer 池化正为此服务)。
//
// ⚠ 与其它算子一样,静态注册要求本档案以 whole-archive 链入(CMake 已配置)。
#include <lmflow/opencl.hpp>

namespace {
using OclUploadKernel = lmflow::ocl::UploadKernel;
using OclDownloadKernel = lmflow::ocl::DownloadKernel;
}  // namespace

LMFLOW_REGISTER_KERNEL_AS(OclUploadKernel, "OclUpload")
LMFLOW_REGISTER_KERNEL_AS(OclDownloadKernel, "OclDownload")
