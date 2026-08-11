#include <lmflow/vulkan.hpp>

// 只验证装好的 SDK 能被外部工程 include + 链接;不要求本机有 Vulkan 设备。
int main() {
  const int64_t shape[1] = {4};
  try {
    lmflow::vk::Image image =
        lmflow::vk::Image::Allocate(lmflow::vk::Context::Shared(), LMFLOW_DTYPE_F32, 1, shape);
    return image.valid() ? 0 : 1;
  } catch (const std::exception&) {
    return 0;  // 无设备:链接通过即算成功
  }
}
