/*
 * vulkan.hpp —— 可选 Vulkan compute adapter:让连续的 GPU 段把中间结果留在设备上。
 *
 * **不属于 ABI,也不是 core 的依赖。** 只有需要 GPU 的算子才 include 本文件;
 * 引擎与 flow.h / flow.hpp 均不依赖 Vulkan,所以没装 Vulkan 也能编译整个 core。
 *
 * 与 adapters/opencl 是**同一形态**(Context::Shared / Image / Upload / Download /
 * EnqueueUnary),把一个算子从一边移植到另一边是机械工作。刻意做成两个独立的 payload
 * 类型而不是统一抽象,因为 compute shader 本身就是后端特有的(OpenCL C 源码 vs SPIR-V),
 * 统一类型只会掩盖一个掩盖不了的差异;而类型分开还顺带保证**一张图不可能混用两种后端**
 * —— 接错在建图期就被端口类型校验拒绝。
 *
 * 与 OpenCL 版的三处实质差异:
 *
 *   1. `EnqueueUnary` 收的是**已编译的 SPIR-V 字节**,不是源码 —— adapter 不该把
 *      glslang/shaderc 拖进依赖。VkShaderModule / pipeline / descriptor layout 按
 *      (SPIR-V, entry) 缓存,与 OpenCL 侧缓存 cl_program 对称。
 *
 *   2. 同步点是**时间线信号量**上的一个值,而不是 cl_event。全部工作走同一队列、同一条
 *      时间线:每次提交等待上一个值、签发下一个值,于是 GPU→GPU 由设备侧串联,
 *      **CPU 线程不阻塞**;只有 Download 做主机侧等待。
 *
 *   3. 显存要自己选堆。若存在同时 DEVICE_LOCAL 且 HOST_VISIBLE 的内存类型,上传/下载
 *      直接映射,**零拷贝**;否则 Upload 退回 staging buffer + 队列拷贝,而
 *      **Download 的 staging 回读尚未实现** —— 那类设备上 `VkDownload` 会在 **Open 期**
 *      就明确失败,不会跑到出帧时才抛。
 *
 *      ⚠ 别把「否则」读成「独显」。判据只是「**存不存在**这样一个内存类型」。
 *      **已实测**:18 台移动 GPU 与 llvmpipe 全部存在,故一律走映射路径。
 *      **尚属推断**(手头独显驱动坏着,无法在 Vulkan 层面证实):经典独显即便没有
 *      Resizable BAR 也应暴露一个 —— 那个传统 256 MB 的 PCIe BAR 窗口(ReBAR 改变的是
 *      它多大,不是它在不在);旁证是本机 RTX 5080 的 BAR1 可调范围为 64 MB…16 GB。
 *      若该推断成立,staging 分支的触发条件就是「设备完全没有 DEVICE_LOCAL|HOST_VISIBLE
 *      类型」,比「独显」窄得多;若不成立,则独显会走 staging,那条路的重要性要重估。
 *      拿到一台驱动正常的独显时,请以实测为准(详见 Upload 里的标注)。
 *
 *      独显上真正要留意的反而是另一件事:既然会挑中那个 host-visible 类型,所有中间
 *      compute buffer 就都落进 BAR 窗口 —— 无 ReBAR 时只有 256 MB(几个 24 MB 的中间
 *      buffer 就能吃满),而且 compute 读写它要过 PCIe,比纯 VRAM 慢。这一条**尚未在
 *      真独显上测过**。
 *
 * 算子注册:本文件只提供算子**类**,不做注册 —— 在你自己的**某一个** .cc 里写
 *
 *   using VkUploadKernel   = lmflow::vk::UploadKernel;
 *   using VkDownloadKernel = lmflow::vk::DownloadKernel;
 *   LMFLOW_REGISTER_KERNEL_AS(VkUploadKernel,   "VkUpload")
 *   LMFLOW_REGISTER_KERNEL_AS(VkDownloadKernel, "VkDownload")
 *
 * 以免头文件被多个 TU 包含时重复注册。必须先起别名:该宏会把类型名拼进标识符
 * (`LMFlowReg_##T`),带 `::` 的限定名无法通过。
 *
 * ---- Android:需要 API level ≥ 31 ----
 *
 * 上面第 2 点那套时间线信号量是 **Vulkan 1.2 core**,而 Android 的 NDK 为每个 API level
 * 提供各自的 `libvulkan.so` 存根,低版本**不导出**这些入口。实测(NDK r27c):
 *
 *   API 21–23   连 libvulkan.so 都没有(Vulkan 自 API 24 才进 NDK)
 *   API 24–27   有库,但缺 vkGetPhysicalDeviceFeatures2 与两个时间线函数
 *   API 28–30   有 vkGetPhysicalDeviceFeatures2,仍缺时间线函数
 *   API 31+     vkWaitSemaphores / vkGetSemaphoreCounterValue 齐全 ✓
 *
 * 所以在 Android 上链接本 adapter,`ANDROID_PLATFORM` 必须 ≥ **android-31**;否则是
 * **链接期**失败(`undefined symbol: vkWaitSemaphores`),不是运行期回退。注意这比仓库
 * 其它 Android 产物的基线高:`cross-android` 的 core 构建与 examples/android 的 JNI
 * 示例都用 android-21 —— 那些不含本 adapter,不受影响。
 *
 * 这里要分清两件**不同**的事,别混为一谈:
 *
 *   编译期底线 —— 由 NDK 存根导出哪些符号决定,就是上面那张表(≥ android-31)。
 *   运行期可用 —— 由设备的**驱动**决定,与编译时选的 API level 无关。
 *
 * 后者是 `HasTimelineSemaphore` 与构造函数末尾那两道闸门在管的,判据也在那里说明:
 * `timelineSemaphore` 特性位在 18 台真机上**恒为 TRUE**,一台都没报过 FALSE,所以它
 * 单独用毫无鉴别力;`apiVersion >= 1.2` 与 `vkGetDeviceProcAddr` 才管用。
 * 那两道闸门是**为了不崩**(否则会跳空指针),不是为了降低编译期底线 —— 换掉特性查询
 * 一点也降不了底线:三个符号里查询只占一个,另外两个(`vkWaitSemaphores` 在
 * WaitTimeline、`vkGetSemaphoreCounterValue` 在 ReclaimCompletedLocked)是运行期真在
 * 干活的时间线 API,没有 1.0 等价物,低版本存根里照样缺席,链接照样失败。
 *
 * 顺带记一条容易走弯路的:1.0 的 `vkGetPhysicalDeviceFeatures`(那个固定结构)**无论
 * 如何都读不出** timeline 支持 —— `timelineSemaphore` 只存在于
 * `...TimelineSemaphoreFeatures` / `...Vulkan12Features`,而这两个结构只能经 `Features2`
 * 的 pNext 链取到。能替代它的是 `vkGetPhysicalDeviceProperties` 的 `apiVersion`。
 *
 * 真正能降编译期底线的只有**运行期取符号**(`vkGetInstanceProcAddr` /
 * `vkGetDeviceProcAddr` 取全部三个并**通过指针调用**),那样编译期不链接任何 1.1/1.2
 * 入口。本 adapter 目前只用 procAddr 做**检测**,没有改成通过指针调用,所以底线仍是
 * android-31。走 `VK_KHR_timeline_semaphore` 扩展也不行:`vkWaitSemaphoresKHR` 之类
 * 在低版本存根里同样缺席,一样得动态取。
 */
#ifndef LMFLOW_VULKAN_HPP_
#define LMFLOW_VULKAN_HPP_

#include <vulkan/vulkan.h>

#include <cstring>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include <lmflow/flow.hpp>

namespace lmflow {
namespace vk {

/// Image 支持的最大维数,与 OpenCL adapter 一致。
constexpr int kMaxNdim = 4;

/// 单个元素的字节数。直接用 C ABI 自带的表(未知 dtype 返回 0),不在 adapter 里另抄一份
/// —— 抄的那份此前漏了 LMFLOW_DTYPE_I64 与 LMFLOW_DTYPE_F16,遇到就抛。
inline size_t DtypeSize(int32_t dtype) {
  const size_t size = lmflow_dtype_size(dtype);
  if (size == 0) {
    throw std::invalid_argument("flow/vk: dtype has no known element size");
  }
  return size;
}

inline void Check(VkResult status, const char* what) {
  if (status != VK_SUCCESS) {
    throw std::runtime_error(std::string("flow/vk: ") + what + " failed with VkResult " +
                             std::to_string(static_cast<int>(status)));
  }
}

class Image;

/// 进程级共享的设备上下文。
///
/// 生命周期挂在**进程**上,而不是图上 —— 宿主若把参数烤进图、参数一变即重建图,
/// 图级所有权会导致每次重建都重造设备并重建 pipeline。Image 各自持有一份 shared_ptr,
/// 因此上下文不会早于仍存活的设备内存被销毁。
class Context {
 public:
  Context(const Context&) = delete;
  Context& operator=(const Context&) = delete;

  ~Context() {
    if (device_) {
      vkDeviceWaitIdle(device_);
      ReclaimUpTo(UINT64_MAX);
      for (auto& entry : programs_) {
        vkDestroyPipeline(device_, entry.second.pipeline, nullptr);
        vkDestroyPipelineLayout(device_, entry.second.pipeline_layout, nullptr);
        vkDestroyDescriptorSetLayout(device_, entry.second.set_layout, nullptr);
        vkDestroyShaderModule(device_, entry.second.module, nullptr);
      }
      if (descriptor_pool_) vkDestroyDescriptorPool(device_, descriptor_pool_, nullptr);
      if (command_pool_) vkDestroyCommandPool(device_, command_pool_, nullptr);
      if (timeline_) vkDestroySemaphore(device_, timeline_, nullptr);
      vkDestroyDevice(device_, nullptr);
    }
    if (instance_) vkDestroyInstance(instance_, nullptr);
  }

  static const std::shared_ptr<Context>& Shared() {
    static const std::shared_ptr<Context> shared{new Context()};
    return shared;
  }

  VkDevice device() const { return device_; }
  VkPhysicalDevice physical_device() const { return physical_; }
  VkQueue queue() const { return queue_; }

  /// 是否存在同时 DEVICE_LOCAL 且 HOST_VISIBLE 的内存类型。
  ///
  /// ARM 统一内存与软件实现为真 —— 此时上传/下载直接映射,不需要 staging。
  bool unified_memory() const { return unified_memory_; }

  /// 单队列 + 单时间线,故「分配命令缓冲 + 记录 + 提交」整段由这把锁保护。
  ///
  /// 注意:正因为只有一条队列且提交串行,**每线程 command pool 在这里买不到东西** ——
  /// 那是引入多队列/并行记录之后才需要的。这里用一个池并由本锁保护,更简单也更少资源。
  std::mutex& submit_mutex() { return submit_mutex_; }

  /// 没有匹配类型时的返回值(`FindMemoryTypeOrNone`)。
  static constexpr uint32_t kNoMemoryType = UINT32_MAX;

  /// 找第一个满足 `want` 的内存类型;没有则返回 `kNoMemoryType`,不抛。
  /// 用于「优先某属性、允许退而求其次」的两段式选择。
  uint32_t FindMemoryTypeOrNone(uint32_t type_bits, VkMemoryPropertyFlags want) const {
    for (uint32_t i = 0; i < memory_properties_.memoryTypeCount; ++i) {
      if ((type_bits & (1u << i)) == 0) continue;
      if ((memory_properties_.memoryTypes[i].propertyFlags & want) == want) return i;
    }
    return kNoMemoryType;
  }

  uint32_t FindMemoryType(uint32_t type_bits, VkMemoryPropertyFlags want) const {
    const uint32_t index = FindMemoryTypeOrNone(type_bits, want);
    if (index == kNoMemoryType) {
      throw std::runtime_error("flow/vk: no memory type satisfies the requested properties");
    }
    return index;
  }

  /// 某内存类型的属性位。零拷贝下载要据此判断映射出的内存是否 HOST_CACHED ——
  /// 未缓存/写合并内存(独显的 PCIe BAR 窗口就是这种)交给下游 CPU 算子读,
  /// 每次访存都走总线,比一次 bulk memcpy 慢得多,那种设备上必须退回拷贝路径。
  VkMemoryPropertyFlags memory_type_flags(uint32_t index) const {
    return index < memory_properties_.memoryTypeCount
               ? memory_properties_.memoryTypes[index].propertyFlags
               : 0;
  }

  /// 一次 dispatch 用完就要回收的资源。按时间线值延迟回收,**不做主机等待**。
  struct Retired {
    uint64_t value = 0;
    VkCommandBuffer command_buffer = VK_NULL_HANDLE;
    VkDescriptorSet descriptor_set = VK_NULL_HANDLE;
    VkBuffer buffer = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
  };

  /// 登记「等这个时间线值过了就能释放」。调用方须持有 submit_mutex()。
  void RetireLocked(const Retired& retired) { retired_.push_back(retired); }

  /// 回收所有已完成的登记项。调用方须持有 submit_mutex()。
  void ReclaimCompletedLocked() {
    uint64_t completed = 0;
    if (vkGetSemaphoreCounterValue(device_, timeline_, &completed) != VK_SUCCESS) return;
    ReclaimUpTo(completed);
  }

  /// 阻塞到某个时间线值完成 —— 只在 Download(GPU→CPU 边界)使用。
  void WaitTimeline(uint64_t value) const {
    if (value == 0) return;
    VkSemaphoreWaitInfo wait{};
    wait.sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO;
    wait.semaphoreCount = 1;
    wait.pSemaphores = &timeline_;
    wait.pValues = &value;
    Check(vkWaitSemaphores(device_, &wait, UINT64_MAX), "vkWaitSemaphores");
  }

  struct Program {
    VkShaderModule module = VK_NULL_HANDLE;
    VkDescriptorSetLayout set_layout = VK_NULL_HANDLE;
    VkPipelineLayout pipeline_layout = VK_NULL_HANDLE;
    VkPipeline pipeline = VK_NULL_HANDLE;
  };

  /// 取一个编译好的 compute pipeline;按 (SPIR-V, entry, push 常量大小) 缓存。
  ///
  /// 缓存在进程级上下文里,所以图重建会命中,不会重复建 pipeline(毫秒级)。
  const Program& ProgramFor(const uint32_t* spirv, size_t spirv_words, const std::string& entry,
                           uint32_t push_constant_bytes) {
    std::lock_guard<std::mutex> guard(cache_mutex_);
    std::string key(reinterpret_cast<const char*>(spirv), spirv_words * sizeof(uint32_t));
    key += '\0';
    key += entry;
    key += '\0';
    key += std::to_string(push_constant_bytes);
    auto cached = programs_.find(key);
    if (cached != programs_.end()) return cached->second;

    Program program;
    VkShaderModuleCreateInfo module_info{};
    module_info.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO;
    module_info.codeSize = spirv_words * sizeof(uint32_t);
    module_info.pCode = spirv;
    Check(vkCreateShaderModule(device_, &module_info, nullptr, &program.module),
          "vkCreateShaderModule");

    VkDescriptorSetLayoutBinding bindings[2]{};
    for (uint32_t i = 0; i < 2; ++i) {
      bindings[i].binding = i;
      bindings[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
      bindings[i].descriptorCount = 1;
      bindings[i].stageFlags = VK_SHADER_STAGE_COMPUTE_BIT;
    }
    VkDescriptorSetLayoutCreateInfo set_info{};
    set_info.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO;
    set_info.bindingCount = 2;
    set_info.pBindings = bindings;
    Check(vkCreateDescriptorSetLayout(device_, &set_info, nullptr, &program.set_layout),
          "vkCreateDescriptorSetLayout");

    VkPushConstantRange push{};
    push.stageFlags = VK_SHADER_STAGE_COMPUTE_BIT;
    push.offset = 0;
    push.size = push_constant_bytes;
    VkPipelineLayoutCreateInfo layout_info{};
    layout_info.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO;
    layout_info.setLayoutCount = 1;
    layout_info.pSetLayouts = &program.set_layout;
    layout_info.pushConstantRangeCount = push_constant_bytes ? 1 : 0;
    layout_info.pPushConstantRanges = push_constant_bytes ? &push : nullptr;
    Check(vkCreatePipelineLayout(device_, &layout_info, nullptr, &program.pipeline_layout),
          "vkCreatePipelineLayout");

    VkComputePipelineCreateInfo pipeline_info{};
    pipeline_info.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO;
    pipeline_info.stage.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
    pipeline_info.stage.stage = VK_SHADER_STAGE_COMPUTE_BIT;
    pipeline_info.stage.module = program.module;
    pipeline_info.stage.pName = entry.c_str();
    pipeline_info.layout = program.pipeline_layout;
    Check(vkCreateComputePipelines(device_, VK_NULL_HANDLE, 1, &pipeline_info, nullptr,
                                   &program.pipeline),
          "vkCreateComputePipelines");

    return programs_.emplace(std::move(key), program).first->second;
  }

  /// 只分配命令缓冲(拷贝类提交不需要 descriptor set)。调用方须持有 submit_mutex()。
  void AllocateCommandLocked(VkCommandBuffer* command_buffer) {
    VkCommandBufferAllocateInfo command_info{};
    command_info.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
    command_info.commandPool = command_pool_;
    command_info.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    command_info.commandBufferCount = 1;
    Check(vkAllocateCommandBuffers(device_, &command_info, command_buffer),
          "vkAllocateCommandBuffers");
  }

  /// 分配命令缓冲与 descriptor set。调用方须持有 submit_mutex()。
  void AllocateLocked(VkDescriptorSetLayout set_layout, VkCommandBuffer* command_buffer,
                      VkDescriptorSet* descriptor_set) {
    AllocateCommandLocked(command_buffer);

    VkDescriptorSetAllocateInfo set_alloc{};
    set_alloc.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO;
    set_alloc.descriptorPool = descriptor_pool_;
    set_alloc.descriptorSetCount = 1;
    set_alloc.pSetLayouts = &set_layout;
    Check(vkAllocateDescriptorSets(device_, &set_alloc, descriptor_set),
          "vkAllocateDescriptorSets");
  }

  /// 已签发的最大时间线值。任何**可能**引用某块显存的提交,其签发值都 ≤ 本值
  /// (提交与 Reset 同受 submit_mutex 保护,不会并发),故按本值登记延迟回收,
  /// 就能保证「归还晚于所有在途工作」。调用方须持有 submit_mutex()。
  uint64_t last_issued_locked() const { return timeline_value_; }

  /// 提交一个已记录好的命令缓冲:等待上一个时间线值,签发新值。
  /// 调用方须持有 submit_mutex()。返回本次签发的值。
  uint64_t SubmitLocked(VkCommandBuffer command_buffer, uint64_t wait_value) {
    const uint64_t signal_value = ++timeline_value_;
    VkTimelineSemaphoreSubmitInfo timeline{};
    timeline.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO;
    timeline.waitSemaphoreValueCount = wait_value ? 1 : 0;
    timeline.pWaitSemaphoreValues = wait_value ? &wait_value : nullptr;
    timeline.signalSemaphoreValueCount = 1;
    timeline.pSignalSemaphoreValues = &signal_value;

    const VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT;
    VkSubmitInfo submit{};
    submit.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
    submit.pNext = &timeline;
    submit.waitSemaphoreCount = wait_value ? 1 : 0;
    submit.pWaitSemaphores = wait_value ? &timeline_ : nullptr;
    submit.pWaitDstStageMask = wait_value ? &wait_stage : nullptr;
    submit.commandBufferCount = 1;
    submit.pCommandBuffers = &command_buffer;
    submit.signalSemaphoreCount = 1;
    submit.pSignalSemaphores = &timeline_;
    Check(vkQueueSubmit(queue_, 1, &submit, VK_NULL_HANDLE), "vkQueueSubmit");
    return signal_value;
  }

 private:
  Context() {
    VkApplicationInfo app{};
    app.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
    // 1.2 起时间线信号量进核心;本 adapter 依赖它做 GPU 侧串联。
    app.apiVersion = VK_API_VERSION_1_2;
    VkInstanceCreateInfo instance_info{};
    instance_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    instance_info.pApplicationInfo = &app;
    Check(vkCreateInstance(&instance_info, nullptr, &instance_), "vkCreateInstance");

    uint32_t device_count = 0;
    Check(vkEnumeratePhysicalDevices(instance_, &device_count, nullptr),
          "vkEnumeratePhysicalDevices");
    if (device_count == 0) throw std::runtime_error("flow/vk: no Vulkan physical device");
    std::vector<VkPhysicalDevice> devices(device_count);
    Check(vkEnumeratePhysicalDevices(instance_, &device_count, devices.data()),
          "vkEnumeratePhysicalDevices");

    // 优先独显/集显,退而接受软件实现(便于无 GPU 的 CI 也能验功能)。
    const VkPhysicalDeviceType preferred[] = {VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU,
                                              VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU,
                                              VK_PHYSICAL_DEVICE_TYPE_CPU};
    for (VkPhysicalDeviceType want : preferred) {
      for (VkPhysicalDevice candidate : devices) {
        VkPhysicalDeviceProperties properties{};
        vkGetPhysicalDeviceProperties(candidate, &properties);
        if (properties.deviceType != want) continue;
        if (!FindComputeQueueFamily(candidate, &queue_family_)) continue;
        if (!HasTimelineSemaphore(candidate)) continue;
        physical_ = candidate;
        break;
      }
      if (physical_) break;
    }
    if (!physical_) {
      // 最后一搏:任意带 compute 队列且支持时间线信号量的设备。
      for (VkPhysicalDevice candidate : devices) {
        if (FindComputeQueueFamily(candidate, &queue_family_) &&
            HasTimelineSemaphore(candidate)) {
          physical_ = candidate;
          break;
        }
      }
    }
    if (!physical_) {
      throw std::runtime_error(
          "flow/vk: no device with a compute queue and timeline semaphore support");
    }

    const float priority = 1.0f;
    VkDeviceQueueCreateInfo queue_info{};
    queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    queue_info.queueFamilyIndex = queue_family_;
    queue_info.queueCount = 1;
    queue_info.pQueuePriorities = &priority;
    VkPhysicalDeviceTimelineSemaphoreFeatures timeline_features{};
    timeline_features.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES;
    timeline_features.timelineSemaphore = VK_TRUE;
    VkDeviceCreateInfo device_info{};
    device_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    device_info.pNext = &timeline_features;
    device_info.queueCreateInfoCount = 1;
    device_info.pQueueCreateInfos = &queue_info;
    Check(vkCreateDevice(physical_, &device_info, nullptr, &device_), "vkCreateDevice");
    vkGetDeviceQueue(device_, queue_family_, 0, &queue_);

    // 最后一道闸门,也是**权威**判据:特性位与 apiVersion 都过了,仍要确认入口点真的接上。
    // 驱动只报告支持而不实现是真实存在的(见 HasTimelineSemaphore 的实测记录),而那种
    // 情况下继续跑就是在第一次 vkWaitSemaphores / vkGetSemaphoreCounterValue 时跳空指针。
    // 宁可在构造期带着可操作的消息失败。
    //
    // 注:改成**通过这些指针调用**还能顺带降低编译期 API 底线(见文件头 Android 一节),
    // 但那是另一件事;这里只做检测,不改调用方式。
    if (vkGetDeviceProcAddr(device_, "vkGetSemaphoreCounterValue") == nullptr ||
        vkGetDeviceProcAddr(device_, "vkWaitSemaphores") == nullptr) {
      vkDestroyDevice(device_, nullptr);
      device_ = VK_NULL_HANDLE;
      throw std::runtime_error(
          "flow/vk: the driver reports timeline semaphore support but does not provide "
          "vkWaitSemaphores / vkGetSemaphoreCounterValue; this adapter cannot run on it");
    }

    vkGetPhysicalDeviceMemoryProperties(physical_, &memory_properties_);
    unified_memory_ = false;
    for (uint32_t i = 0; i < memory_properties_.memoryTypeCount; ++i) {
      const VkMemoryPropertyFlags flags = memory_properties_.memoryTypes[i].propertyFlags;
      if ((flags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) &&
          (flags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)) {
        unified_memory_ = true;
        break;
      }
    }

    VkSemaphoreTypeCreateInfo semaphore_type{};
    semaphore_type.sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO;
    semaphore_type.semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE;
    semaphore_type.initialValue = 0;
    VkSemaphoreCreateInfo semaphore_info{};
    semaphore_info.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO;
    semaphore_info.pNext = &semaphore_type;
    Check(vkCreateSemaphore(device_, &semaphore_info, nullptr, &timeline_), "vkCreateSemaphore");

    VkCommandPoolCreateInfo pool_info{};
    pool_info.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
    pool_info.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pool_info.queueFamilyIndex = queue_family_;
    Check(vkCreateCommandPool(device_, &pool_info, nullptr, &command_pool_),
          "vkCreateCommandPool");

    VkDescriptorPoolSize size{};
    size.type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
    size.descriptorCount = kMaxSets * 2;
    VkDescriptorPoolCreateInfo descriptor_info{};
    descriptor_info.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO;
    descriptor_info.flags = VK_DESCRIPTOR_POOL_CREATE_FREE_DESCRIPTOR_SET_BIT;
    descriptor_info.maxSets = kMaxSets;
    descriptor_info.poolSizeCount = 1;
    descriptor_info.pPoolSizes = &size;
    Check(vkCreateDescriptorPool(device_, &descriptor_info, nullptr, &descriptor_pool_),
          "vkCreateDescriptorPool");
  }

  static bool FindComputeQueueFamily(VkPhysicalDevice device, uint32_t* family) {
    uint32_t count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(device, &count, nullptr);
    std::vector<VkQueueFamilyProperties> families(count);
    vkGetPhysicalDeviceQueueFamilyProperties(device, &count, families.data());
    for (uint32_t i = 0; i < count; ++i) {
      if (families[i].queueFlags & VK_QUEUE_COMPUTE_BIT) {
        *family = i;
        return true;
      }
    }
    return false;
  }

  /// 设备是否**真的**能用时间线信号量。
  ///
  /// ⚠ 不能只看特性位。18 台真机实测:`timelineSemaphore` 特性位**恒为 TRUE**,
  /// 一台都没报 FALSE —— 包括一台 `apiVersion` 只有 1.1、且
  /// `vkGetDeviceProcAddr("vkWaitSemaphores")` 返回 **NULL** 的 Adreno 710(SM6450)。
  /// 在那台机器上 `vkCreateDevice` 带着该特性也会**成功**,于是崩溃被推迟到第一次真调用:
  /// 平台 libvulkan 导出了符号、但派发表那一项是空的,于是跳到地址 0 → SIGSEGV
  /// (栈:vkGetSemaphoreCounterValue+16 ← Image::Reset)。
  ///
  /// 所以这里加 `apiVersion >= 1.2` 这道闸门:timeline semaphore 是 Vulkan 1.2 的 core
  /// 必备特性,实测在那 18 台上该判定与 procAddr 的可用性**完全一致**(唯一失败的机型
  /// 也正是唯一 apiVersion < 1.2 的)。构造函数末尾还会再验一次函数指针,那是权威判据。
  ///
  /// 同型号并不等价:SM6450 上的 Adreno 710 是 api 1.1(不可用),SM6435 上的同款是
  /// api 1.3(可用)—— 决定因素是驱动/平台版本,故机型白名单不可行,必须运行期判。
  static bool HasTimelineSemaphore(VkPhysicalDevice device) {
    VkPhysicalDeviceProperties properties{};
    vkGetPhysicalDeviceProperties(device, &properties);
    if (properties.apiVersion < VK_API_VERSION_1_2) return false;
    VkPhysicalDeviceTimelineSemaphoreFeatures timeline{};
    timeline.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES;
    VkPhysicalDeviceFeatures2 features{};
    features.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
    features.pNext = &timeline;
    vkGetPhysicalDeviceFeatures2(device, &features);
    return timeline.timelineSemaphore == VK_TRUE;
  }

  void ReclaimUpTo(uint64_t completed) {
    size_t keep = 0;
    for (size_t i = 0; i < retired_.size(); ++i) {
      const Retired& entry = retired_[i];
      if (entry.value > completed) {
        retired_[keep++] = entry;
        continue;
      }
      if (entry.command_buffer) vkFreeCommandBuffers(device_, command_pool_, 1, &entry.command_buffer);
      if (entry.descriptor_set) vkFreeDescriptorSets(device_, descriptor_pool_, 1, &entry.descriptor_set);
      if (entry.buffer) vkDestroyBuffer(device_, entry.buffer, nullptr);
      if (entry.memory) vkFreeMemory(device_, entry.memory, nullptr);
    }
    retired_.resize(keep);
  }

  static constexpr uint32_t kMaxSets = 1024;

  VkInstance instance_ = VK_NULL_HANDLE;
  VkPhysicalDevice physical_ = VK_NULL_HANDLE;
  VkDevice device_ = VK_NULL_HANDLE;
  VkQueue queue_ = VK_NULL_HANDLE;
  uint32_t queue_family_ = 0;
  VkPhysicalDeviceMemoryProperties memory_properties_{};
  bool unified_memory_ = false;
  VkSemaphore timeline_ = VK_NULL_HANDLE;
  uint64_t timeline_value_ = 0;
  VkCommandPool command_pool_ = VK_NULL_HANDLE;
  VkDescriptorPool descriptor_pool_ = VK_NULL_HANDLE;
  std::vector<Retired> retired_;
  std::mutex submit_mutex_;
  std::mutex cache_mutex_;
  std::unordered_map<std::string, Program> programs_;
};

/// 驻留设备的负载(storage buffer)。
///
/// 这是 GPU 段之间流动的 payload 类型。它**不是** LMFlowBuffer —— 端口类型检查会在
/// 建图期拒绝把它接到 CPU 算子上。
///
/// `ready` 是生产者在时间线上签发的值:消费者提交时以它为等待值(设备侧等待,CPU 不阻塞),
/// 只有 Download 才做主机侧等待。
///
/// 移动语义、不可复制 —— 一个 Image 唯一拥有其 buffer/memory。扇出时引擎按引用共享同一
/// payload(只读),不复制。
class Image {
 public:
  Image() = default;

  Image(Image&& other) noexcept { MoveFrom(std::move(other)); }
  Image& operator=(Image&& other) noexcept {
    if (this != &other) {
      Reset();
      MoveFrom(std::move(other));
    }
    return *this;
  }
  Image(const Image&) = delete;
  Image& operator=(const Image&) = delete;
  ~Image() { Reset(); }

  /// 在设备上分配一块 storage buffer。
  ///
  /// 优先 DEVICE_LOCAL;若该类型同时 HOST_VISIBLE(ARM 统一内存/软件实现),上传下载
  /// 就能直接映射而不需要 staging。
  static Image Allocate(const std::shared_ptr<Context>& context, int32_t dtype, int ndim,
                        const int64_t* shape) {
    if (ndim <= 0 || ndim > kMaxNdim) {
      throw std::invalid_argument("flow/vk: Image ndim must be within [1, 4]");
    }
    size_t count = 1;
    for (int i = 0; i < ndim; ++i) {
      if (shape[i] <= 0) throw std::invalid_argument("flow/vk: Image shape must be positive");
      count *= static_cast<size_t>(shape[i]);
    }
    const VkDeviceSize bytes = count * DtypeSize(dtype);

    Image image;
    image.context_ = context;
    image.dtype_ = dtype;
    image.ndim_ = ndim;
    for (int i = 0; i < ndim; ++i) image.shape_[i] = shape[i];

    VkBufferCreateInfo buffer_info{};
    buffer_info.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO;
    buffer_info.size = bytes;
    buffer_info.usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
                        VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT;
    buffer_info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    Check(vkCreateBuffer(context->device(), &buffer_info, nullptr, &image.buffer_),
          "vkCreateBuffer");

    VkMemoryRequirements requirements{};
    vkGetBufferMemoryRequirements(context->device(), image.buffer_, &requirements);
    VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT;
    // 统一内存上按三档优先级挑,而不是碰运气,也不要求 cached 与 coherent 兼备:
    //
    //   ① cached + coherent —— 最好,零拷贝且无需缓存维护
    //   ② cached,非 coherent —— 仍可零拷贝,但读写要显式 invalidate / flush
    //   ③ 只有 coherent(未缓存)—— 退回拷贝路径
    //
    // ②这一档是必须的:实测 5 种 Mali(G52/G57/G615/G625/G720)**都没有** cached+coherent
    // 的类型,只有 cached 非 coherent 的。若像先前那样要求两者兼备,等于在所有 Mali 上
    // 静默关掉零拷贝下载(分配照样成功、结果照样对,只是白白多一整次回读拷贝)。
    //
    // 显式排序同样必要:18 台真机上未缓存的 HOST_VISIBLE|COHERENT 类型**都排在**缓存版
    // 之前,所以「取第一个匹配」在 memoryTypeBits 放行它时就会选中未缓存的那个。
    uint32_t type_index = Context::kNoMemoryType;
    if (context->unified_memory()) {
      const VkMemoryPropertyFlags visible = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT;
      const VkMemoryPropertyFlags coherent = VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
      const VkMemoryPropertyFlags cached = VK_MEMORY_PROPERTY_HOST_CACHED_BIT;
      type_index = context->FindMemoryTypeOrNone(requirements.memoryTypeBits,
                                                 want | visible | coherent | cached);
      if (type_index == Context::kNoMemoryType) {
        type_index = context->FindMemoryTypeOrNone(requirements.memoryTypeBits,
                                                  want | visible | cached);
      }
      if (type_index == Context::kNoMemoryType) {
        type_index = context->FindMemoryTypeOrNone(requirements.memoryTypeBits,
                                                  want | visible | coherent);
      }
    }
    if (type_index == Context::kNoMemoryType) {
      type_index = context->FindMemoryType(requirements.memoryTypeBits, want);
    }
    VkMemoryAllocateInfo allocate{};
    allocate.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    allocate.allocationSize = requirements.size;
    allocate.memoryTypeIndex = type_index;
    Check(vkAllocateMemory(context->device(), &allocate, nullptr, &image.memory_),
          "vkAllocateMemory");
    Check(vkBindBufferMemory(context->device(), image.buffer_, image.memory_, 0),
          "vkBindBufferMemory");
    // 三个标志都由**实际挑中的**内存类型推出,而不是由 unified_memory() 推断 ——
    // 落到第 ④ 档(纯 device-local)时 host_visible 必须是 false。
    const VkMemoryPropertyFlags picked = context->memory_type_flags(type_index);
    image.host_visible_ = (picked & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) != 0;
    image.host_cached_ = (picked & VK_MEMORY_PROPERTY_HOST_CACHED_BIT) != 0;
    image.host_coherent_ = (picked & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) != 0;
    image.mapped_bytes_ = requirements.size;
    return image;
  }

  bool valid() const { return buffer_ != VK_NULL_HANDLE; }
  const std::shared_ptr<Context>& context() const { return context_; }
  VkBuffer buffer() const { return buffer_; }
  VkDeviceMemory memory() const { return memory_; }
  bool host_visible() const { return host_visible_; }
  /// 映射出的内存是否既 HOST_CACHED 又 HOST_COHERENT —— 零拷贝下载的前提。
  bool host_cached() const { return host_cached_; }
  /// 映射出的内存是否 HOST_COHERENT。false 时主机读写前后必须显式 invalidate / flush
  /// (Mali 唯一的缓存类型就是非 coherent 的)。
  bool host_coherent() const { return host_coherent_; }
  /// 生产者签发的时间线值;0 表示无需等待。
  uint64_t ready() const { return ready_; }
  void SetReady(uint64_t value) { ready_ = value; }
  int32_t dtype() const { return dtype_; }
  int ndim() const { return ndim_; }
  int64_t shape(int index) const { return shape_[index]; }

  size_t element_count() const {
    size_t count = 1;
    for (int i = 0; i < ndim_; ++i) count *= static_cast<size_t>(shape_[i]);
    return count;
  }
  size_t byte_size() const { return element_count() * DtypeSize(dtype_); }

  /// 把一次 dispatch 的一次性资源挂到本 Image 上,随它一起延迟回收。
  void RetainDispatch(VkCommandBuffer command_buffer, VkDescriptorSet descriptor_set) {
    command_buffer_ = command_buffer;
    descriptor_set_ = descriptor_set;
  }

 private:
  void Reset() {
    if (!context_) return;
    // 不做主机等待:把资源登记到延迟回收表,由后续提交顺带回收。
    //
    // 登记值必须是**已签发的最大时间线值**,而不是本 Image 自己的 ready_:统一内存路径的
    // Upload 只做主机 memcpy、不提交,ready_ 保持 0,而读它的 dispatch 却可能仍在飞行 ——
    // 若按 ready_=0 登记,ReclaimUpTo 会立刻 vkDestroyBuffer/vkFreeMemory,GPU 便读到已
    // 释放的显存(表现为驱动 worker 线程段错误)。按最大签发值登记才覆盖所有在途引用。
    Context::Retired retired;
    retired.command_buffer = command_buffer_;
    retired.descriptor_set = descriptor_set_;
    retired.buffer = buffer_;
    retired.memory = memory_;
    {
      std::lock_guard<std::mutex> guard(context_->submit_mutex());
      retired.value = context_->last_issued_locked();
      context_->RetireLocked(retired);
      context_->ReclaimCompletedLocked();
    }
    buffer_ = VK_NULL_HANDLE;
    memory_ = VK_NULL_HANDLE;
    command_buffer_ = VK_NULL_HANDLE;
    descriptor_set_ = VK_NULL_HANDLE;
    context_.reset();
  }

  void MoveFrom(Image&& other) noexcept {
    context_ = std::move(other.context_);
    buffer_ = other.buffer_;
    memory_ = other.memory_;
    command_buffer_ = other.command_buffer_;
    descriptor_set_ = other.descriptor_set_;
    ready_ = other.ready_;
    dtype_ = other.dtype_;
    ndim_ = other.ndim_;
    host_visible_ = other.host_visible_;
    host_cached_ = other.host_cached_;
    host_coherent_ = other.host_coherent_;
    mapped_bytes_ = other.mapped_bytes_;
    for (int i = 0; i < kMaxNdim; ++i) shape_[i] = other.shape_[i];
    other.buffer_ = VK_NULL_HANDLE;
    other.memory_ = VK_NULL_HANDLE;
    other.command_buffer_ = VK_NULL_HANDLE;
    other.descriptor_set_ = VK_NULL_HANDLE;
    other.ndim_ = 0;
  }

  std::shared_ptr<Context> context_;
  VkBuffer buffer_ = VK_NULL_HANDLE;
  VkDeviceMemory memory_ = VK_NULL_HANDLE;
  VkCommandBuffer command_buffer_ = VK_NULL_HANDLE;
  VkDescriptorSet descriptor_set_ = VK_NULL_HANDLE;
  uint64_t ready_ = 0;
  int32_t dtype_ = 0;
  int ndim_ = 0;
  bool host_visible_ = false;
  bool host_cached_ = false;
  bool host_coherent_ = false;
  VkDeviceSize mapped_bytes_ = 0;
  int64_t shape_[kMaxNdim] = {0, 0, 0, 0};
};

namespace detail {

/// 把主机内存拷进/拷出一块 host-visible 的设备内存。
/// 把主机内存拷进/拷出一块 host-visible 的设备内存。
///
/// `coherent=false` 时(Mali 唯一的缓存类型就是这样)必须按规范显式做缓存维护:写完
/// flush 才能让设备看见,读前 invalidate 才能让主机看见。漏掉就是静默读到/写出陈旧数据。
///
/// 整块映射(VK_WHOLE_SIZE)而非只映射 bytes:这样 VkMappedMemoryRange 用
/// offset=0 + VK_WHOLE_SIZE 一定落在已映射范围内,也就无需理会 nonCoherentAtomSize
/// 的对齐约束。
inline void CopyMapped(const std::shared_ptr<Context>& context, VkDeviceMemory memory,
                       void* host, size_t bytes, bool to_device, bool coherent) {
  void* mapped = nullptr;
  Check(vkMapMemory(context->device(), memory, 0, VK_WHOLE_SIZE, 0, &mapped), "vkMapMemory");
  VkMappedMemoryRange range{};
  range.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE;
  range.memory = memory;
  range.offset = 0;
  range.size = VK_WHOLE_SIZE;
  if (to_device) {
    std::memcpy(mapped, host, bytes);
    if (!coherent) {
      Check(vkFlushMappedMemoryRanges(context->device(), 1, &range),
            "vkFlushMappedMemoryRanges");
    }
  } else {
    if (!coherent) {
      Check(vkInvalidateMappedMemoryRanges(context->device(), 1, &range),
            "vkInvalidateMappedMemoryRanges");
    }
    std::memcpy(host, mapped, bytes);
  }
  vkUnmapMemory(context->device(), memory);
}

}  // namespace detail

namespace detail {

/// 零拷贝下载交给引擎的释放状态。
///
/// 输出包**可能比产生它的 Image 活得久**(扇出、下游缓存),所以这里必须连**持有 Image 的
/// 那个输入包**一起留住:它析构才会走 Image::Reset 去按时间线退还显存。unmap 必须先于
/// 那次析构发生,故两件事在同一个回调里按序做。
struct MappedHandoff {
  std::shared_ptr<Context> context;
  VkDeviceMemory memory = VK_NULL_HANDLE;
  Packet owner;
};

/// 引擎在最后一个包引用消失时调用一次;可能落在任意工作线程上。
inline void ReleaseMappedHandoff(void* user_data) {
  auto* state = static_cast<MappedHandoff*>(user_data);
  if (state->context && state->memory != VK_NULL_HANDLE) {
    vkUnmapMemory(state->context->device(), state->memory);
  }
  delete state;  // owner 在此析构 —— 显存随之进入延迟回收表
}

}  // namespace detail

/// 把一个 1~4 维 LMFlowBuffer 上传到设备。
///
/// 统一内存(ARM / 软件实现)直接映射拷贝,**不经 staging**;独显则走 staging buffer
/// 加一次队列拷贝。两种情况都是 CPU→GPU 边界,阻塞在此是预期的。
inline Image Upload(const std::shared_ptr<Context>& context, const LMFlowBuffer& buffer) {
  if (buffer.ndim <= 0 || buffer.ndim > kMaxNdim) {
    throw std::invalid_argument("flow/vk: Upload supports buffers with ndim within [1, 4]");
  }
  if (!BufferIsContiguous(buffer)) {
    throw std::invalid_argument(
        "flow/vk: Upload needs a row-major contiguous buffer, but the descriptor is strided "
        "(a padded cv::Mat row or a sliced numpy view looks like this); pack it first");
  }
  Image image = Image::Allocate(context, buffer.dtype, buffer.ndim, buffer.shape);
  const size_t bytes = image.byte_size();

  if (image.host_visible()) {
    detail::CopyMapped(context, image.memory(), buffer.data, bytes, /*to_device=*/true,
                       image.host_coherent());
    return image;
  }

  // ⚠ 以下 staging 路径**从未在任何测试过的设备上执行过** —— 未验证代码,改动请当心。
  //
  // 触发条件是 `host_visible()` 为假,也就是**设备完全没有** DEVICE_LOCAL|HOST_VISIBLE
  // 的内存类型。实测 18 台移动 GPU(Adreno 613/710/722/740/810/812/829/830/840/850 与
  // Mali G52/G57/G615/G625/G720)加 llvmpipe,**全部**存在这样的类型,于是一律走上面的
  // 映射路径。**至于独显则尚未证实**:预期它也不例外(即便没有 Resizable BAR,仍应暴露
  // 那个传统 256 MB 的 PCIe BAR 窗口,故「关掉 ReBAR」造不出这个场景),但手头独显的
  // Vulkan 驱动版本错配、无法枚举,所以这一条是**推断而非实测**。也就是说:目前既没有
  // 已验证的触发实例,也不能断言独显一定不触发。
  //
  // 要验证它,需要一台确实没有该内存类型的设备;`adapters/vulkan/benchmarks` 之外还有个
  // 更直接的办法:把 vkGetPhysicalDeviceMemoryProperties 的输出打出来,看有没有同时带
  // 这两个位的类型。在拿到这样一台设备之前,这段代码保留但**不应被信任**:它含一次内存
  // 分配、一个命令缓冲、一次队列提交和一次延迟回收登记 —— 而本 adapter 历史上的
  // use-after-free 正是出在同一套延迟回收机制里。
  //
  // 另需注意:即使它被执行,`Download` 在这类设备上也会在 Open 期就被拒(staging 回读
  // 未实现),故完整 GPU→CPU 回路仍不可用;只有纯 GPU 段(上传 + 计算)用得到它。
  //
  // staging buffer -> vkCmdCopyBuffer。staging 随本次提交延迟回收。
  VkBuffer staging = VK_NULL_HANDLE;
  VkDeviceMemory staging_memory = VK_NULL_HANDLE;
  VkBufferCreateInfo staging_info{};
  staging_info.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO;
  staging_info.size = bytes;
  staging_info.usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT;
  Check(vkCreateBuffer(context->device(), &staging_info, nullptr, &staging), "vkCreateBuffer");
  VkMemoryRequirements requirements{};
  vkGetBufferMemoryRequirements(context->device(), staging, &requirements);
  VkMemoryAllocateInfo allocate{};
  allocate.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
  allocate.allocationSize = requirements.size;
  allocate.memoryTypeIndex =
      context->FindMemoryType(requirements.memoryTypeBits,
                              VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                  VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
  Check(vkAllocateMemory(context->device(), &allocate, nullptr, &staging_memory),
        "vkAllocateMemory");
  Check(vkBindBufferMemory(context->device(), staging, staging_memory, 0), "vkBindBufferMemory");
  detail::CopyMapped(context, staging_memory, buffer.data, bytes, /*to_device=*/true,
                     /*coherent=*/true);  // staging 就是按 HOST_COHERENT 分配的

  uint64_t signalled = 0;
  {
    std::lock_guard<std::mutex> guard(context->submit_mutex());
    context->ReclaimCompletedLocked();
    VkCommandBuffer command_buffer = VK_NULL_HANDLE;
    context->AllocateCommandLocked(&command_buffer);
    VkCommandBufferBeginInfo begin{};
    begin.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    Check(vkBeginCommandBuffer(command_buffer, &begin), "vkBeginCommandBuffer");
    VkBufferCopy region{};
    region.size = bytes;
    vkCmdCopyBuffer(command_buffer, staging, image.buffer(), 1, &region);
    Check(vkEndCommandBuffer(command_buffer), "vkEndCommandBuffer");
    signalled = context->SubmitLocked(command_buffer, /*wait_value=*/0);
    Context::Retired retired;
    retired.value = signalled;
    retired.command_buffer = command_buffer;
    retired.buffer = staging;
    retired.memory = staging_memory;
    context->RetireLocked(retired);
  }
  image.SetReady(signalled);
  return image;
}

/// 把设备负载下载成一个引擎分配的 LMFlowBuffer 包。
///
/// 这是整条链上唯一需要主机等待的地方 —— 先等生产者的时间线值,再读回。
inline Packet Download(const Image& image) {
  const std::shared_ptr<Context>& context = image.context();
  // 先判设备能力、再分配输出包:device-only 显存没有回读路径,提前失败也省掉一次白分配。
  if (!image.host_visible()) {
    throw std::runtime_error(
        "flow/vk: Download needs a DEVICE_LOCAL|HOST_VISIBLE memory type; this device has none "
        "(typical for a discrete GPU) and the staging read-back path is not implemented");
  }

  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  for (int i = 0; i < image.ndim(); ++i) shape[i] = image.shape(i);
  // 免初始化分配:回读会把整块写满 —— 输出包按同一 ndim/shape/dtype 分配,其字节数就等于
  // image.byte_size(),而下面的 CopyMapped 正好拷这么多字节,故满足「emit 前写满」的契约。
  // 用保证清零的 new_buffer 等于每帧白做一次全缓冲 memset(24 MB 量级约 0.4 ms 起)。
  LMFlowBuffer out{};
  Packet packet = Packet::NewBufferUninitialized(image.ndim(), shape, image.dtype(), &out);
  if (packet.IsEmpty()) {
    throw std::runtime_error("flow/vk: lmflow_packet_new_buffer_uninit failed");
  }

  context->WaitTimeline(image.ready());
  detail::CopyMapped(context, image.memory(), out.data, image.byte_size(), /*to_device=*/false,
                     image.host_coherent());
  return packet;
}

/// 零拷贝下载:把已映射的设备内存**直接交给引擎**,不再拷进一个新包。
///
/// 适用条件由 `CanDownloadMapped` 判定,必须先问再调:内存要既 HOST_VISIBLE 又
/// **HOST_CACHED**(ARM 统一内存与软件实现如此)。未缓存/写合并内存 —— 独显的 PCIe BAR
/// 窗口正是这种 —— 绝不能走这条路:下游 CPU 算子的每次访存都会过总线,比一次 bulk
/// memcpy 慢得多,那种设备上 `Download()` 的拷贝反而是对的。
///
/// `image_packet` 是持有 vk::Image 的包,并在此**移交所有权**:引擎会一直持有它,直到
/// 下游放掉最后一个引用,才 unmap 并把显存交还延迟回收表。
///
/// 交出的视图标了 READONLY —— 它是设备内存的视图,扇出时多个下游共享同一份;想就地改写
/// 的下游会照常走 CoW 复制一份,不会写穿到显存上。
inline Packet DownloadMapped(Packet image_packet) {
  const Image* image = image_packet.TryGet<Image>();
  if (!image || !image->valid()) {
    throw std::invalid_argument("flow/vk: DownloadMapped needs a valid vk::Image packet");
  }
  if (!image->host_visible() || !image->host_cached()) {
    throw std::invalid_argument(
        "flow/vk: DownloadMapped needs host-cached, host-coherent device memory; check "
        "CanDownloadMapped() first and fall back to Download()");
  }

  // 先把要用的东西取成值,后面 image_packet 会被移走,不再依赖 image 指针。
  std::shared_ptr<Context> context = image->context();
  const VkDeviceMemory memory = image->memory();
  const int ndim = image->ndim();
  const int32_t dtype = image->dtype();
  const size_t bytes = image->byte_size();
  const bool coherent = image->host_coherent();
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  for (int i = 0; i < ndim; ++i) shape[i] = image->shape(i);

  // 交出去之前必须等生产者写完 —— 之后读的是下游 CPU 算子,引擎不会替我们同步。
  context->WaitTimeline(image->ready());

  // 整块映射,便于下面用 offset=0 + VK_WHOLE_SIZE 做缓存维护(无需管 nonCoherentAtomSize)。
  void* mapped = nullptr;
  Check(vkMapMemory(context->device(), memory, 0, VK_WHOLE_SIZE, 0, &mapped), "vkMapMemory");
  if (!coherent) {
    // 非 coherent 内存(Mali 唯一的缓存类型)必须先 invalidate,否则主机读到的是陈旧
    // 缓存行。这里只需做一次:GPU 已经写完(上面等过时间线),而交出的视图是 READONLY,
    // 此后不会再有设备侧写入。
    VkMappedMemoryRange range{};
    range.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE;
    range.memory = memory;
    range.offset = 0;
    range.size = VK_WHOLE_SIZE;
    Check(vkInvalidateMappedMemoryRanges(context->device(), 1, &range),
          "vkInvalidateMappedMemoryRanges");
  }

  LMFlowBuffer view{};
  view.data = mapped;
  view.ndim = ndim;
  view.dtype = dtype;
  int64_t stride = static_cast<int64_t>(DtypeSize(dtype));
  for (int i = ndim - 1; i >= 0; --i) {
    view.shape[i] = shape[i];
    view.strides[i] = stride;
    stride *= shape[i];
  }
  view.flags = LMFLOW_BUF_FLAG_READONLY;

  auto* state = new detail::MappedHandoff{context, memory, std::move(image_packet)};
  Packet adopted = Packet::AdoptBuffer(view, detail::ReleaseMappedHandoff, state);
  if (adopted.IsEmpty()) {
    // 契约:失败时 release_fn 不会被调用,所有权仍归我们,自己收拾干净。
    vkUnmapMemory(context->device(), memory);
    state->memory = VK_NULL_HANDLE;
    delete state;
    throw std::runtime_error("flow/vk: lmflow_packet_adopt_buffer rejected the mapped view");
  }
  return adopted;
}

/// 本 Image 能否走零拷贝下载。见 `DownloadMapped` 的条件说明。
inline bool CanDownloadMapped(const Image& image) {
  return image.valid() && image.host_visible() && image.host_cached();
}

/// 把一个一维 dispatch 入队,产出新的设备负载。
///
/// 生产者的时间线值作为等待值、新值记到输出上 —— **CPU 线程不阻塞**,这就是连续 GPU
/// 段不落主机的机制。`push_constants` 直接透传给 shader 的 push constant 块。
/// 一次 1 输入 1 输出 dispatch 里**因算子而异**的部分。
///
/// 各算子之间真正不同的只有两样:输出的形状/类型,以及工作规模 —— 其余(分配命令缓冲与
/// descriptor set、把 src/dst 写进 binding 0/1、推 push constants、按生产者时间线值提交、
/// 记录同步点与延迟回收)完全一致。把这两样参数化,算子里就不必再抄一遍那四十来行。
///
/// 全部留空即"输出与输入同形同类型、按输出元素数铺 1 维",也就是 `EnqueueUnary` 的行为。
///
/// `global` 是**要覆盖的工作项数**,不是工作组数 —— 组数由本函数按 `local_size` 向上取整,
/// 免得每个算子各自做一遍除法(算错就是覆盖不全或越界)。`local_size` 必须与 shader 里
/// `layout(local_size_...)` 声明的一致。
struct DispatchSpec {
  int32_t dtype = 0;                      ///< 0 = 沿用输入的 dtype
  int ndim = 0;                           ///< 0 = 沿用输入的 ndim/shape
  const int64_t* shape = nullptr;         ///< ndim 非 0 时必填
  int work_dim = 0;                       ///< 0 = 1 维,按输出元素数
  uint32_t global[3] = {0, 0, 0};         ///< 要覆盖的工作项数
  uint32_t local_size[3] = {64, 1, 1};    ///< 须与 shader 的 local_size 一致
};

/// 把一个 compute dispatch 入队,产出新的设备负载。
///
/// 生产者的时间线值作为等待值、新值记到输出上 —— **CPU 线程不阻塞**,这就是连续 GPU 段
/// 不落主机的机制。`push_constants` 直接透传给 shader 的 push constant 块。
///
/// 只覆盖「1 输入 1 输出」:`ProgramFor` 建的 descriptor layout 固定是两个 storage buffer,
/// 多输入算子需要另建 layout,不适用本函数。
inline Image Enqueue(const Image& input, const DispatchSpec& spec, const uint32_t* spirv,
                     size_t spirv_words, const std::string& entry, const void* push_constants,
                     uint32_t push_constant_bytes) {
  const std::shared_ptr<Context>& context = input.context();
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  const int out_ndim = spec.ndim != 0 ? spec.ndim : input.ndim();
  if (spec.ndim != 0) {
    if (spec.shape == nullptr) {
      throw std::invalid_argument("flow/vk: DispatchSpec.ndim set but shape is null");
    }
    if (spec.ndim < 0 || spec.ndim > kMaxNdim) {
      throw std::invalid_argument("flow/vk: DispatchSpec.ndim must be within [1, 4]");
    }
    for (int i = 0; i < spec.ndim; ++i) shape[i] = spec.shape[i];
  } else {
    for (int i = 0; i < input.ndim(); ++i) shape[i] = input.shape(i);
  }
  Image output =
      Image::Allocate(context, spec.dtype != 0 ? spec.dtype : input.dtype(), out_ndim, shape);

  const Context::Program& program =
      context->ProgramFor(spirv, spirv_words, entry, push_constant_bytes);

  // 默认按**输出**元素数铺 1 维 —— 写的是输出,输入可能更大或更小。
  uint32_t global[3] = {static_cast<uint32_t>(output.element_count()), 1, 1};
  uint32_t local[3] = {spec.local_size[0], spec.local_size[1], spec.local_size[2]};
  const int work_dim = spec.work_dim != 0 ? spec.work_dim : 1;
  if (spec.work_dim != 0) {
    for (int i = 0; i < work_dim; ++i) global[i] = spec.global[i];
  }
  uint32_t groups[3] = {1, 1, 1};
  for (int i = 0; i < 3; ++i) {
    const uint32_t l = local[i] != 0 ? local[i] : 1;
    groups[i] = (global[i] + l - 1) / l;
    if (groups[i] == 0) groups[i] = 1;
  }

  uint64_t signalled = 0;
  VkCommandBuffer command_buffer = VK_NULL_HANDLE;
  VkDescriptorSet descriptor_set = VK_NULL_HANDLE;
  {
    std::lock_guard<std::mutex> guard(context->submit_mutex());
    context->ReclaimCompletedLocked();
    context->AllocateLocked(program.set_layout, &command_buffer, &descriptor_set);

    VkDescriptorBufferInfo buffers[2]{};
    buffers[0].buffer = input.buffer();
    buffers[0].range = VK_WHOLE_SIZE;
    buffers[1].buffer = output.buffer();
    buffers[1].range = VK_WHOLE_SIZE;
    VkWriteDescriptorSet writes[2]{};
    for (uint32_t i = 0; i < 2; ++i) {
      writes[i].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
      writes[i].dstSet = descriptor_set;
      writes[i].dstBinding = i;
      writes[i].descriptorCount = 1;
      writes[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
      writes[i].pBufferInfo = &buffers[i];
    }
    vkUpdateDescriptorSets(context->device(), 2, writes, 0, nullptr);

    VkCommandBufferBeginInfo begin{};
    begin.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    Check(vkBeginCommandBuffer(command_buffer, &begin), "vkBeginCommandBuffer");
    vkCmdBindPipeline(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE, program.pipeline);
    vkCmdBindDescriptorSets(command_buffer, VK_PIPELINE_BIND_POINT_COMPUTE,
                            program.pipeline_layout, 0, 1, &descriptor_set, 0, nullptr);
    if (push_constant_bytes) {
      vkCmdPushConstants(command_buffer, program.pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0,
                         push_constant_bytes, push_constants);
    }
    vkCmdDispatch(command_buffer, groups[0], groups[1], groups[2]);
    Check(vkEndCommandBuffer(command_buffer), "vkEndCommandBuffer");

    signalled = context->SubmitLocked(command_buffer, input.ready());
  }
  output.SetReady(signalled);
  output.RetainDispatch(command_buffer, descriptor_set);
  return output;
}

/// `Enqueue` 的薄封装:输出与输入同形同类型,按元素数铺 1 维。
inline Image EnqueueUnary(const Image& input, const uint32_t* spirv, size_t spirv_words,
                          const std::string& entry, const void* push_constants,
                          uint32_t push_constant_bytes, uint32_t local_size_x = 64) {
  DispatchSpec spec;
  spec.local_size[0] = local_size_x;
  return Enqueue(input, spec, spirv, spirv_words, entry, push_constants, push_constant_bytes);
}

/// LMFlowBuffer -> Image。注册名建议 "VkUpload"。
class UploadKernel : public Kernel {
 public:
  static void GetContract(Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
    c.OutputSet<Image>(0);
  }

  Status Process(lmflow::Context& cc) override {
    Packet input = cc.TakeInput(0);
    LMFlowBuffer buffer{};
    if (!input.AsBuffer(&buffer)) return cc.Fail("VkUpload expects an LMFlowBuffer input");
    cc.Emit(0, Packet::Make<Image>(Upload(Context::Shared(), buffer)));
    return Status::Ok();
  }
};

/// Image -> LMFlowBuffer。注册名建议 "VkDownload"。
class DownloadKernel : public Kernel {
 public:
  static void GetContract(Contract& c) {
    c.InputSet<Image>(0);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
  }

  /// 不支持的设备在 **Open 期**就失败:device-only 显存没有 staging 回读路径,与其跑到
  /// 出帧时才抛,不如让 `graph.start()` 直接报错(「算子应在 Open 里直接失败」见 flow.hpp)。
  Status Open(lmflow::Context& cc) override {
    if (!Context::Shared()->unified_memory()) {
      return cc.Fail(
          "VkDownload: this device has no DEVICE_LOCAL|HOST_VISIBLE memory type (typical for a "
          "discrete GPU); the staging read-back path is not implemented");
    }
    return Status::Ok();
  }

  Status Process(lmflow::Context& cc) override {
    Packet input = cc.TakeInput(0);
    const Image* image = input.TryGet<Image>();
    if (!image || !image->valid()) return cc.Fail("VkDownload expects a vk::Image input");
    // 主机缓存的统一内存上,把映射直接交给引擎,省掉整次回读拷贝;其余设备照常拷。
    if (CanDownloadMapped(*image)) {
      cc.Emit(0, DownloadMapped(std::move(input)));
    } else {
      cc.Emit(0, Download(*image));
    }
    return Status::Ok();
  }
};

}  // namespace vk
}  // namespace lmflow

LMFLOW_DECLARE_TYPE_NAME(lmflow::vk::Image, "lmflow.vulkan.Image")

#endif  // LMFLOW_VULKAN_HPP_
