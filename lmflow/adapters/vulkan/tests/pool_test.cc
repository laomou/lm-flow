// lmflow Vulkan adapter —— buffer 池的行为测试。
//
// 池化的唯一收益是「稳态尺寸下不再每帧向驱动要内存」。那个收益此前**没有任何自动化覆盖**:
// resize 测试只校验数值,所以把池改成永不命中、或把淘汰策略写反,测试照样全绿。这个文件就是
// 把那个指标钉下来。
//
//   g++ -std=c++17 -Iinclude -Iadapters/vulkan/include
//       adapters/vulkan/tests/pool_test.cc core/target/release/liblmflow.a
//       -lvulkan -lpthread -ldl -lm -o lmflow_vulkan_pool_test
//
// 注意 Context 是**进程级单例**,计数器跨用例累积,所以每个用例都取增量而非绝对值。

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <vector>

#include "lmflow/vulkan.hpp"

#define CHECK(condition)                                                   \
  do {                                                                     \
    if (!(condition)) {                                                    \
      std::fprintf(stderr, "CHECK failed at %s:%d: %s\n", __FILE__,        \
                   __LINE__, #condition);                                  \
      return EXIT_FAILURE;                                                 \
    }                                                                      \
  } while (false)

namespace {

using lmflow::vk::Context;
using lmflow::vk::Image;

/// 分配一块 `elements` 个 f32 的计算 buffer 然后立刻丢掉 —— 于是它会经延迟回收归还池。
void Churn(const std::shared_ptr<Context>& context, int64_t elements) {
  const int64_t shape[1] = {elements};
  Image image = Image::Allocate(context, LMFLOW_DTYPE_F32, 1, shape);
  (void)image;
}

}  // namespace

int main() {
  std::shared_ptr<Context> context;
  try {
    context = Context::Shared();
  } catch (const std::exception& e) {
    std::printf("skipping: no usable Vulkan device (%s)\n", e.what());
    return 0;
  }

  std::printf("Vulkan buffer 池行为:\n");

  // ── 1. 稳态尺寸 ⇒ 只在第一次向驱动要内存 ────────────────────────────────
  //
  // 这是池化的收益指标本身。修复前后都应通过 —— 它守的是「别把池改成永不命中」。
  {
    const int64_t kElements = 4096;
    Churn(context, kElements);  // 预热:第一次必然是新分配
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

  // ── 2. 尺寸单调增长后重复最大尺寸 ⇒ 仍应命中 ──────────────────────────
  //
  // 这是**淘汰策略的回归测试**。池满时若无条件丢掉刚归还的那个(而不是淘汰最小的),池就被
  // 一堆再也匹配不上的小 buffer 占满,最大尺寸每轮都要重新分配 —— 下面的断言会失败。
  {
    const int64_t kBase = 8192;
    const int kGrow = static_cast<int>(Context::kMaxPooledSlots) + 4;  // 确保撑满并溢出
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
    CHECK(allocations == 0);  // 最大的那块必须留在池里
  }

  // ── 3. 更大的请求不得复用更小的 buffer ────────────────────────────────
  //
  // 复用门槛按 buffer 大小判断(见 Context::BufferSlot):容量不足必须走新分配,而不是把一块
  // 更小的 VkBuffer 当成更大的用。注意这条在**不做 padding 的驱动上无法暴露**当初那个
  // 「capacity 记成 requirements.size」的 bug —— 那需要 requirements.size > info.size。
  {
    const Context::PoolStats before = context->image_pool_stats();
    Churn(context, 1024);              // 池里现在有一块 4 KiB
    Churn(context, 1024 * 1024);       // 远大于它 → 必须新分配
    const Context::PoolStats after = context->image_pool_stats();
    std::printf("  ok  : 超出池内最大容量的请求 → 新分配 %llu 次\n",
                (unsigned long long)(after.allocations - before.allocations));
    CHECK(after.allocations - before.allocations >= 1);
  }

  // ── 4. 池的两个上限都不被突破 ────────────────────────────────────────
  {
    const Context::PoolStats stats = context->image_pool_stats();
    std::printf("  ok  : 池占用 %zu 槽 / %llu 字节(上限 %zu 槽 / %llu 字节)\n", stats.slots,
                (unsigned long long)stats.bytes, Context::kMaxPooledSlots,
                (unsigned long long)Context::kMaxPooledBytes);
    CHECK(stats.slots <= Context::kMaxPooledSlots);
    CHECK(stats.bytes <= Context::kMaxPooledBytes);
  }

  std::printf("全部通过\n");
  return EXIT_SUCCESS;
}
