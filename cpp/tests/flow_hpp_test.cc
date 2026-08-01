// flow.hpp C++ 糖层的独立单元测试 —— 只依赖头文件,不需链接引擎。
//
// 守卫的核心不变量:C++ 算子的任何异常都**不得穿越 extern "C" 回到 Rust 引擎**
// (那是 UB;Rust 的 catch_unwind 接不住 C++ 异常),且 create 失败(返回 nullptr)
// 后 open/process/close 必须安全返回错误而不是 null 解引用。
//
//   g++ -std=c++17 -Wall -Wextra -Werror -Iinclude cpp/flow_hpp_test.cc -o flow_hpp_test

#include <cassert>
#include <cstdio>
#include <stdexcept>

#include "flow.hpp"

namespace {

// 构造函数抛异常(如打开设备失败):create 必须接住并返回 nullptr。
struct ThrowingCtorKernel : lmflow::Kernel {
  ThrowingCtorKernel() { throw std::runtime_error("ctor boom"); }
  lmflow::Status Process(lmflow::Context&) override { return lmflow::Status::Ok(); }
};

// Process 抛异常:必须转成 LMFLOW_ERR_KERNEL。
struct ThrowingProcessKernel : lmflow::Kernel {
  lmflow::Status Process(lmflow::Context&) override { throw std::runtime_error("process boom"); }
};

// 正常算子:Process 不碰 Context,便于纯头文件测试(无需引擎符号)。
struct OkKernel : lmflow::Kernel {
  lmflow::Status Process(lmflow::Context&) override { return lmflow::Status::Ok(); }
};

}  // namespace

int main() {
  // 1) 构造抛异常:create 返回 nullptr,绝不让异常穿越 extern "C"。
  {
    const LmflowKernelVTable* vt = lmflow::KernelAdapter<ThrowingCtorKernel>::vtable();
    void* self = vt->create(nullptr);
    assert(self == nullptr && "构造抛异常时 create 必须返回 nullptr");
  }

  // 2) create 失败后 self==nullptr:open/process/close 必须安全返回错误(不 null 解引用)。
  {
    const LmflowKernelVTable* vt = lmflow::KernelAdapter<OkKernel>::vtable();
    assert(vt->open(nullptr, nullptr) == LMFLOW_ERR_KERNEL);
    assert(vt->process(nullptr, nullptr) == LMFLOW_ERR_KERNEL);
    assert(vt->close(nullptr, nullptr) == LMFLOW_ERR_KERNEL);
  }

  // 3) Process 抛异常:process 转成 LMFLOW_ERR_KERNEL。
  {
    const LmflowKernelVTable* vt = lmflow::KernelAdapter<ThrowingProcessKernel>::vtable();
    void* self = vt->create(nullptr);
    assert(self != nullptr);
    assert(vt->process(self, nullptr) == LMFLOW_ERR_KERNEL);
    vt->destroy(self);
  }

  // 4) 正常算子:create 非空,process 返回 LMFLOW_OK。
  {
    const LmflowKernelVTable* vt = lmflow::KernelAdapter<OkKernel>::vtable();
    void* self = vt->create(nullptr);
    assert(self != nullptr);
    assert(vt->process(self, nullptr) == LMFLOW_OK);
    vt->destroy(self);
  }

  std::printf("flow.hpp 单元测试全部通过\n");
  return 0;
}
