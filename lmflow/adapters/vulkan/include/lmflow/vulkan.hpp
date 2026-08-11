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
 *   3. 显存要自己选堆。若存在同时 DEVICE_LOCAL 且 HOST_VISIBLE 的内存类型(ARM 统一内存
 *      与软件实现都是如此),上传/下载直接映射,**零拷贝**;否则退回 staging buffer + 队列
 *      拷贝(独显)。两条路径都实现了。
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

inline size_t DtypeSize(int32_t dtype) {
  switch (dtype) {
    case LMFLOW_DTYPE_U8:
    case LMFLOW_DTYPE_I8: return 1;
    case LMFLOW_DTYPE_U16:
    case LMFLOW_DTYPE_I16: return 2;
    case LMFLOW_DTYPE_I32:
    case LMFLOW_DTYPE_F32: return 4;
    case LMFLOW_DTYPE_F64: return 8;
    default:
      throw std::invalid_argument("flow/vk: dtype has no known element size");
  }
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

  uint32_t FindMemoryType(uint32_t type_bits, VkMemoryPropertyFlags want) const {
    for (uint32_t i = 0; i < memory_properties_.memoryTypeCount; ++i) {
      if ((type_bits & (1u << i)) == 0) continue;
      if ((memory_properties_.memoryTypes[i].propertyFlags & want) == want) return i;
    }
    throw std::runtime_error("flow/vk: no memory type satisfies the requested properties");
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

  static bool HasTimelineSemaphore(VkPhysicalDevice device) {
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
    if (context->unified_memory()) want |= VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                          VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    VkMemoryAllocateInfo allocate{};
    allocate.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    allocate.allocationSize = requirements.size;
    allocate.memoryTypeIndex = context->FindMemoryType(requirements.memoryTypeBits, want);
    Check(vkAllocateMemory(context->device(), &allocate, nullptr, &image.memory_),
          "vkAllocateMemory");
    Check(vkBindBufferMemory(context->device(), image.buffer_, image.memory_, 0),
          "vkBindBufferMemory");
    image.host_visible_ = context->unified_memory();
    return image;
  }

  bool valid() const { return buffer_ != VK_NULL_HANDLE; }
  const std::shared_ptr<Context>& context() const { return context_; }
  VkBuffer buffer() const { return buffer_; }
  VkDeviceMemory memory() const { return memory_; }
  bool host_visible() const { return host_visible_; }
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
    Context::Retired retired;
    retired.value = ready_;
    retired.command_buffer = command_buffer_;
    retired.descriptor_set = descriptor_set_;
    retired.buffer = buffer_;
    retired.memory = memory_;
    {
      std::lock_guard<std::mutex> guard(context_->submit_mutex());
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
  int64_t shape_[kMaxNdim] = {0, 0, 0, 0};
};

namespace detail {

/// 把主机内存拷进/拷出一块 host-visible 的设备内存。
inline void CopyMapped(const std::shared_ptr<Context>& context, VkDeviceMemory memory,
                       void* host, size_t bytes, bool to_device) {
  void* mapped = nullptr;
  Check(vkMapMemory(context->device(), memory, 0, bytes, 0, &mapped), "vkMapMemory");
  if (to_device) {
    std::memcpy(mapped, host, bytes);
  } else {
    std::memcpy(host, mapped, bytes);
  }
  vkUnmapMemory(context->device(), memory);
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
  Image image = Image::Allocate(context, buffer.dtype, buffer.ndim, buffer.shape);
  const size_t bytes = image.byte_size();

  if (image.host_visible()) {
    detail::CopyMapped(context, image.memory(), buffer.data, bytes, /*to_device=*/true);
    return image;
  }

  // 独显路径:staging buffer -> vkCmdCopyBuffer。staging 随本次提交延迟回收。
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
  detail::CopyMapped(context, staging_memory, buffer.data, bytes, /*to_device=*/true);

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
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  for (int i = 0; i < image.ndim(); ++i) shape[i] = image.shape(i);
  LMFlowBuffer out{};
  Packet packet = Packet::Adopt(
      lmflow_packet_new_buffer(image.ndim(), shape, image.dtype(), LMFLOW_TS_UNSET, &out));
  if (packet.IsEmpty()) throw std::runtime_error("flow/vk: lmflow_packet_new_buffer failed");

  const std::shared_ptr<Context>& context = image.context();
  context->WaitTimeline(image.ready());
  if (image.host_visible()) {
    detail::CopyMapped(context, image.memory(), out.data, image.byte_size(),
                       /*to_device=*/false);
    return packet;
  }
  throw std::runtime_error(
      "flow/vk: Download from device-only memory needs a staging path; not implemented");
}

/// 把一个一维 dispatch 入队,产出新的设备负载。
///
/// 生产者的时间线值作为等待值、新值记到输出上 —— **CPU 线程不阻塞**,这就是连续 GPU
/// 段不落主机的机制。`push_constants` 直接透传给 shader 的 push constant 块。
inline Image EnqueueUnary(const Image& input, const uint32_t* spirv, size_t spirv_words,
                          const std::string& entry, const void* push_constants,
                          uint32_t push_constant_bytes, uint32_t local_size_x = 64) {
  const std::shared_ptr<Context>& context = input.context();
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  for (int i = 0; i < input.ndim(); ++i) shape[i] = input.shape(i);
  Image output = Image::Allocate(context, input.dtype(), input.ndim(), shape);

  const Context::Program& program =
      context->ProgramFor(spirv, spirv_words, entry, push_constant_bytes);
  const size_t count = input.element_count();
  const uint32_t groups =
      static_cast<uint32_t>((count + local_size_x - 1) / local_size_x);

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
      vkCmdPushConstants(command_buffer, program.pipeline_layout,
                         VK_SHADER_STAGE_COMPUTE_BIT, 0, push_constant_bytes, push_constants);
    }
    vkCmdDispatch(command_buffer, groups, 1, 1);
    Check(vkEndCommandBuffer(command_buffer), "vkEndCommandBuffer");

    signalled = context->SubmitLocked(command_buffer, input.ready());
  }
  output.SetReady(signalled);
  output.RetainDispatch(command_buffer, descriptor_set);
  return output;
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

  Status Process(lmflow::Context& cc) override {
    Packet input = cc.TakeInput(0);
    const Image* image = input.TryGet<Image>();
    if (!image || !image->valid()) return cc.Fail("VkDownload expects a vk::Image input");
    cc.Emit(0, Download(*image));
    return Status::Ok();
  }
};

}  // namespace vk
}  // namespace lmflow

LMFLOW_DECLARE_TYPE_NAME(lmflow::vk::Image, "lmflow.vulkan.Image")

#endif  // LMFLOW_VULKAN_HPP_
