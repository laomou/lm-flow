// lmflow OpenCL adapter —— buffer 池的行为测试。与 adapters/vulkan/tests/pool_test.cc 同构。
//
// 池化的唯一收益是「稳态尺寸下不再每帧向驱动要内存」,而这个收益此前没有任何自动化覆盖:
// 数值测试不关心分配了几次,所以把池改成永不命中、或把淘汰策略写反,测试照样全绿。
//
// 除收益指标外,这里还守住一条**正确性**约束:复用必须要求 cl_mem_flags **完全相等**。
// `Image::Allocate` 的 flags 是公开、调用方可控的参数,若只按容量匹配,宿主用
// CL_MEM_READ_ONLY 分配的 buffer 会被一次默认 READ_WRITE 请求复用去当 kernel 输出 ——
// 按 OpenCL 规范是未定义行为,而且完全静默。
//
// 注意 Context 是**进程级单例**,计数器跨用例累积,所以每个用例都取增量而非绝对值。

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <memory>

#include "lmflow/opencl.hpp"

#define CHECK(condition)                                                   \
  do {                                                                     \
    if (!(condition)) {                                                    \
      std::fprintf(stderr, "CHECK failed at %s:%d: %s\n", __FILE__,        \
                   __LINE__, #condition);                                  \
      return EXIT_FAILURE;                                                 \
    }                                                                      \
  } while (false)

namespace {

using lmflow::ocl::Context;
using lmflow::ocl::Image;

/// 分配一块 `elements` 个 f32 的计算 buffer 然后立刻丢掉 —— 于是它归还池。
void Churn(const std::shared_ptr<Context>& context, int64_t elements,
           cl_mem_flags flags = CL_MEM_READ_WRITE) {
  const int64_t shape[1] = {elements};
  Image image = Image::Allocate(context, LMFLOW_DTYPE_F32, 1, shape, flags);
  (void)image;
}

}  // namespace

int main() {
  std::shared_ptr<Context> context;
  try {
    context = Context::Shared();
  } catch (const std::exception& e) {
    std::printf("skipping: no usable OpenCL device (%s)\n", e.what());
    return 0;
  }

  std::printf("OpenCL buffer 池行为:\n");

  // ── 1. 稳态尺寸 ⇒ 只在第一次向驱动要内存 ────────────────────────────────
  {
    const int64_t kElements = 4096;
    Churn(context, kElements);  // 预热
    const Context::PoolStats before = context->image_pool_stats();
    const int kRounds = 20;
    for (int i = 0; i < kRounds; ++i) Churn(context, kElements);
    const Context::PoolStats after = context->image_pool_stats();
    const uint64_t allocations = after.allocations - before.allocations;
    const uint64_t reuses = after.reuses - before.reuses;
    std::printf("  ok  : 稳态 %d 轮同尺寸 → 新分配 %llu 次、复用 %llu 次\n", kRounds,
                (unsigned long long)allocations, (unsigned long long)reuses);
    CHECK(allocations == 0);
    CHECK(reuses == static_cast<uint64_t>(kRounds));
  }

  // ── 2. 尺寸单调增长后重复最大尺寸 ⇒ 仍应命中(淘汰策略的回归测试)────────
  {
    const int64_t kBase = 8192;
    const int kGrow = static_cast<int>(Context::kMaxPooledSlots) + 4;
    int64_t largest = 0;
    for (int i = 1; i <= kGrow; ++i) {
      largest = kBase * i;
      Churn(context, largest);
    }
    const Context::PoolStats before = context->image_pool_stats();
    const int kRounds = 5;
    for (int i = 0; i < kRounds; ++i) Churn(context, largest);
    const Context::PoolStats after = context->image_pool_stats();
    const uint64_t allocations = after.allocations - before.allocations;
    std::printf("  ok  : 递增 %d 档撑满池后,重复最大尺寸 %d 轮 → 新分配 %llu 次\n", kGrow,
                kRounds, (unsigned long long)allocations);
    CHECK(allocations == 0);
  }

  // ── 3. flags 不同的 buffer 不得互相复用 ───────────────────────────────
  //
  // 这是**正确性**回归测试,不是收益指标:池若只按容量匹配,下面第二次请求会复用上一块
  // READ_ONLY 的 buffer,于是一块 READ_ONLY 的 cl_mem 被当作可写的计算 buffer 交出去。
  //
  // 尺寸必须比池里现有的**每一块**都大,否则 best-fit 会合法地复用前两个用例留下的
  // READ_WRITE buffer,这一条就测不到东西了(它允许更大的 buffer 服务更小的请求)。
  //
  // 统一内存设备上 Allocate 会给未指定 host-ptr 的请求追加 CL_MEM_ALLOC_HOST_PTR,两次请求
  // 都会被追加,故差异仍只在访问限定位上 —— 正是要区分的那一位。
  {
    const Context::PoolStats pooled = context->image_pool_stats();
    // 比池内总字节还大 ⇒ 必然比池内任何单块都大。
    const int64_t elements = static_cast<int64_t>(pooled.bytes / sizeof(float)) + 4096;
    Churn(context, elements, CL_MEM_READ_ONLY);
    const Context::PoolStats before = context->image_pool_stats();
    Churn(context, elements, CL_MEM_READ_WRITE);
    const Context::PoolStats after = context->image_pool_stats();
    const uint64_t allocations = after.allocations - before.allocations;
    std::printf("  ok  : READ_ONLY 的 buffer 未被同尺寸 READ_WRITE 请求复用 → 新分配 %llu 次\n",
                (unsigned long long)allocations);
    CHECK(allocations == 1);
  }

  // ── 4. 池的两个上限都不被突破 ────────────────────────────────────────
  {
    const Context::PoolStats stats = context->image_pool_stats();
    std::printf("  ok  : 池占用 %zu 槽 / %zu 字节(上限 %zu 槽 / %zu 字节)\n", stats.slots,
                stats.bytes, Context::kMaxPooledSlots, Context::kMaxPooledBytes);
    CHECK(stats.slots <= Context::kMaxPooledSlots);
    CHECK(stats.bytes <= Context::kMaxPooledBytes);
  }

  std::printf("全部通过\n");
  return EXIT_SUCCESS;
}
