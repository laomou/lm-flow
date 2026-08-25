/*
 * opencl.hpp —— 可选 OpenCL adapter:让连续的 GPU 段把中间结果留在设备上。
 *
 * **不属于 ABI,也不是 core 的依赖。** 只有需要 GPU 的算子才 include 本文件;
 * 引擎与 flow.h / flow.hpp 均不依赖 OpenCL,所以没装 OpenCL 也能编译整个 core。
 *
 * 设计要点(与 core 的关系):
 *
 *   1. GPU buffer 是一个**注册的自定义 payload 类型** `ocl::Image`,而不是
 *      `LMFlowBuffer.device` 字段。引擎对 payload 完全不作解释、只按 type_id 做相等性
 *      校验,因此它天然能在图里流动;而端口类型检查会在**建图期**拒绝把 GPU 输出接到
 *      声明了 LMFLOW_TYPE_BUFFER 的 CPU 输入 —— GPU 句柄从不冒充 LMFlowBuffer,
 *      「把设备地址当主机地址读」这类错误从根上不存在。
 *
 *   2. 设备上下文是**进程级**共享(Context::Shared),而不是每个算子各建一份:
 *      cl_mem 属于创建它的 cl_context,跨 context 不可用 —— 若各算子自建,GPU 段之间
 *      每条边都要经主机内存往返。且 clCreateContext + clBuildProgram 是几十到几百毫秒,
 *      图重建时必须命中缓存而不是重做。
 *
 *   3. 同步点随 Image 传递(cl_event)。GPU→GPU 由消费者把生产者的 event 放进
 *      wait list,**CPU 线程不阻塞**;只有 Download(GPU→CPU 边界)才做主机侧等待。
 *      因此连续的 GPU 段不需要引擎支持「任务稍后完成」,Process 保持同步语义即可。
 *
 * 算子注册:本文件只提供算子**类**,不做注册 —— 在你自己的**某一个** .cc 里写
 *
 *   using OclUploadKernel   = lmflow::ocl::UploadKernel;
 *   using OclDownloadKernel = lmflow::ocl::DownloadKernel;
 *   LMFLOW_REGISTER_KERNEL_AS(OclUploadKernel,   "OclUpload")
 *   LMFLOW_REGISTER_KERNEL_AS(OclDownloadKernel, "OclDownload")
 *
 * 以免头文件被多个 TU 包含时重复注册。注意必须先起别名:该宏会把类型名拼进标识符
 * (`LMFlowReg_##T`),带 `::` 的限定名无法通过。
 */
#ifndef LMFLOW_OPENCL_HPP_
#define LMFLOW_OPENCL_HPP_

#ifndef CL_TARGET_OPENCL_VERSION
// 1.2 是移动端的现实下限(Android 上厂商多为 1.2 / 2.0 embedded)。
#define CL_TARGET_OPENCL_VERSION 120
#endif
#include <CL/cl.h>

/// GPU 执行耗时采集开关(默认关)。开启会让每次 dispatch 多约 5µs —— 见 Context 构造处
/// 的实测数据。仅在需要 Image::gpu_duration_ns() 时定义为 1。
#ifndef LMFLOW_OCL_PROFILING
#define LMFLOW_OCL_PROFILING 0
#endif

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
namespace ocl {

/// Image 支持的最大维数。图像类负载 2~3 维足够,留 4 以便带 batch。
constexpr int kMaxNdim = 4;

/// 单个元素的字节数。直接用 C ABI 自带的表(未知 dtype 返回 0),不在 adapter 里另抄一份
/// —— 抄的那份此前漏了 LMFLOW_DTYPE_I64 与 LMFLOW_DTYPE_F16,遇到就抛。
inline size_t DtypeSize(int32_t dtype) {
  const size_t size = lmflow_dtype_size(dtype);
  if (size == 0) {
    throw std::invalid_argument("flow/ocl: dtype has no known element size");
  }
  return size;
}

inline void Check(cl_int status, const char* what) {
  if (status != CL_SUCCESS) {
    throw std::runtime_error(std::string("flow/ocl: ") + what + " failed with " +
                             std::to_string(status));
  }
}

/// 进程级共享的设备上下文。
///
/// 生命周期挂在**进程**上,而不是图上 —— 宿主若把参数烤进图、参数一变即重建图,
/// 图级所有权会导致每次重建都重造设备并重编 kernel(几十到几百毫秒)。
/// Image 各自持有一份 shared_ptr,因此上下文不会早于仍存活的设备内存被销毁。
class Context {
 public:
  Context(const Context&) = delete;
  Context& operator=(const Context&) = delete;

  ~Context() {
    for (auto& entry : kernels_) clReleaseKernel(entry.second);
    for (auto& entry : programs_) clReleaseProgram(entry.second);
    for (const ImageSlot& slot : image_pool_) {
      if (slot.mem) clReleaseMemObject(slot.mem);  // 池里残余的 buffer
    }
    if (queue_) clReleaseCommandQueue(queue_);
    if (context_) clReleaseContext(context_);
  }

  /// 进程级单例。首次调用完成平台/设备/上下文/队列的创建。
  static const std::shared_ptr<Context>& Shared() {
    static const std::shared_ptr<Context> shared{new Context()};
    return shared;
  }

  cl_context context() const { return context_; }
  cl_device_id device() const { return device_; }
  cl_command_queue queue() const { return queue_; }

  /// 设备是否与主机共享内存(ARM/Adreno/Mali 等集成 GPU 为真)。
  ///
  /// 决定 Image 是否用 CL_MEM_ALLOC_HOST_PTR 分配,从而决定 DownloadMapped 能不能省掉
  /// 整次回读拷贝。查的是 CL_DEVICE_HOST_UNIFIED_MEMORY —— 它在 OpenCL 2.0 起标记为
  /// 弃用,但本 adapter 以 1.2 为下限,且移动端厂商实现都还报这个值。
  bool host_unified() const { return host_unified_; }

  /// 队列默认 in-order,且 clSetKernelArg 对同一 cl_kernel 不是线程安全的 ——
  /// 故「设参 + 入队」整段由这把锁保护。GPU 算子跑在普通线程池上,可能并发进入。
  std::mutex& enqueue_mutex() { return enqueue_mutex_; }

  /// 一个可复用的计算 buffer(Image 的设备缓冲)。
  ///
  /// `capacity` 是**创建时**的字节大小。它必须由 Image 一路带着传回来,不能在归还时用
  /// `byte_size()` 重算:一块大 buffer 被小请求复用过之后逻辑大小就小于真实容量了,拿它当
  /// 容量记录会让记录单调变小 —— 大请求再也匹配不上,而那块大内存仍占着槽位,池逐步退化成
  /// 纯开销。
  ///
  /// `flags` 是创建时**实际生效**的 cl_mem_flags,复用要求**完全相等**。只比
  /// CL_MEM_ALLOC_HOST_PTR 远远不够:`Image::Allocate` 的 flags 是公开、调用方可控的参数
  /// (adapter 头的定位就是宿主自己写算子),宿主用 CL_MEM_READ_ONLY 分配的 buffer 若被一次
  /// 默认 READ_WRITE 请求复用去当 kernel 输出,按 OpenCL 规范是未定义行为,而且完全静默;
  /// CL_MEM_HOST_NO_ACCESS 被复用后 clEnqueueReadBuffer 直接非法。host_mapped 也由它推出。
  struct ImageSlot {
    cl_mem mem = nullptr;
    size_t capacity = 0;
    cl_mem_flags flags = 0;
  };

  /// 池的两个上限,都是**天花板而非目标**:池只持有实际被归还过的 buffer。条目数挡住句柄
  /// 累积;字节数挡住大帧下的常驻**设备内存** —— 8 个 24MB 的帧就是 192MB,对移动端不可
  /// 忽略,而单看条目数对此毫无感知。两者取先到者。
  static constexpr size_t kMaxPooledSlots = 8;
  static constexpr size_t kMaxPooledBytes = size_t{256} << 20;

  /// 池的命中统计。稳态尺寸下 `allocations` 应当停止增长 —— 这是池化唯一的收益指标。
  struct PoolStats {
    uint64_t allocations = 0;
    uint64_t reuses = 0;
    size_t slots = 0;
    size_t bytes = 0;
  };
  PoolStats image_pool_stats() {
    std::lock_guard<std::mutex> guard(pool_mutex_);
    return {pool_allocations_, pool_reuses_, image_pool_.size(), pool_bytes_};
  }

  /// Image 池的互斥锁。与 enqueue_mutex_ 分开:Allocate/Reset 可能在任何线程触发
  /// (Reset 常落在包释放回调上,不在入队上下文),池只保护自己的 vector。
  std::mutex& pool_mutex() { return pool_mutex_; }

  /// 从池里 best-fit 取一块容量 >= `bytes`、且 flags 与 `want_flags` **完全相等**的 buffer。
  /// 命中返回 true 并写 `*out`(该 buffer 已出池、为本调用私有);否则返回 false。
  /// 调用方须持有 pool_mutex()。
  bool TryAcquireImageLocked(size_t bytes, cl_mem_flags want_flags, ImageSlot* out) {
    size_t best = SIZE_MAX;
    for (size_t i = 0; i < image_pool_.size(); ++i) {
      const ImageSlot& slot = image_pool_[i];
      if (slot.capacity < bytes || slot.flags != want_flags) continue;
      if (best == SIZE_MAX || slot.capacity < image_pool_[best].capacity) best = i;
    }
    if (best == SIZE_MAX) return false;
    *out = image_pool_[best];
    pool_bytes_ -= out->capacity;
    image_pool_[best] = image_pool_.back();
    image_pool_.pop_back();
    ++pool_reuses_;
    return true;
  }

  /// 记一次新建(未命中池)。调用方须持有 pool_mutex()。
  void NoteImageAllocationLocked() { ++pool_allocations_; }

  /// 把一块 buffer 归还池(替代 clReleaseMemObject)。调用方须持有 pool_mutex()。
  ///
  /// 超限时淘汰**容量最小的那一个** —— 有可能正是刚归还的这个,那也对。反过来(无条件丢掉
  /// 新来者、留住已有的那几个)在尺寸单调增长的负载下会让池彻底失效:池被一堆再也匹配不上的
  /// 小 buffer 占满,每个新的大 buffer 一归还就被释放,于是既永不命中、又白占着设备内存。
  void RecycleImageLocked(const ImageSlot& slot) {
    image_pool_.push_back(slot);
    pool_bytes_ += slot.capacity;
    while (image_pool_.size() > kMaxPooledSlots || pool_bytes_ > kMaxPooledBytes) {
      size_t smallest = 0;
      for (size_t i = 1; i < image_pool_.size(); ++i) {
        if (image_pool_[i].capacity < image_pool_[smallest].capacity) smallest = i;
      }
      pool_bytes_ -= image_pool_[smallest].capacity;
      clReleaseMemObject(image_pool_[smallest].mem);
      image_pool_[smallest] = image_pool_.back();
      image_pool_.pop_back();
    }
  }

  /// 取一个编译好的 kernel;按 (源码, 入口名) 缓存。
  ///
  /// 缓存在进程级上下文里,所以图重建会命中,不会重复 clBuildProgram(毫秒级)。
  cl_kernel KernelFor(const std::string& source, const std::string& entry) {
    std::lock_guard<std::mutex> guard(cache_mutex_);
    const std::string key = entry + '\0' + source;
    auto cached = kernels_.find(key);
    if (cached != kernels_.end()) return cached->second;

    cl_program program = nullptr;
    auto built = programs_.find(source);
    if (built != programs_.end()) {
      program = built->second;
    } else {
      const char* text = source.c_str();
      const size_t length = source.size();
      cl_int status = CL_SUCCESS;
      program = clCreateProgramWithSource(context_, 1, &text, &length, &status);
      Check(status, "clCreateProgramWithSource");
      status = clBuildProgram(program, 1, &device_, nullptr, nullptr, nullptr);
      if (status != CL_SUCCESS) {
        std::string log = BuildLog(program);
        clReleaseProgram(program);
        throw std::runtime_error("flow/ocl: clBuildProgram failed: " + log);
      }
      programs_.emplace(source, program);
    }

    cl_int status = CL_SUCCESS;
    cl_kernel kernel = clCreateKernel(program, entry.c_str(), &status);
    Check(status, "clCreateKernel");
    kernels_.emplace(key, kernel);
    return kernel;
  }

 private:
  Context() {
    cl_uint platform_count = 0;
    Check(clGetPlatformIDs(0, nullptr, &platform_count), "clGetPlatformIDs");
    if (platform_count == 0) throw std::runtime_error("flow/ocl: no OpenCL platform");
    std::vector<cl_platform_id> platforms(platform_count);
    Check(clGetPlatformIDs(platform_count, platforms.data(), nullptr), "clGetPlatformIDs");

    // 优先 GPU,退而求其次接受任意设备(便于在无 GPU 的 CI 上用 CPU ICD 跑通)。
    const cl_device_type wanted[] = {CL_DEVICE_TYPE_GPU, CL_DEVICE_TYPE_ALL};
    for (cl_device_type want : wanted) {
      for (cl_platform_id platform : platforms) {
        cl_uint device_count = 0;
        if (clGetDeviceIDs(platform, want, 0, nullptr, &device_count) != CL_SUCCESS ||
            device_count == 0) {
          continue;
        }
        Check(clGetDeviceIDs(platform, want, 1, &device_, nullptr), "clGetDeviceIDs");
        platform_ = platform;
        break;
      }
      if (device_) break;
    }
    if (!device_) throw std::runtime_error("flow/ocl: no OpenCL device");

    cl_int status = CL_SUCCESS;
    context_ = clCreateContext(nullptr, 1, &device_, nullptr, nullptr, &status);
    Check(status, "clCreateContext");
    // GPU 执行耗时默认**不采集**。开 CL_QUEUE_PROFILING_ENABLE 实测每次 dispatch 要多
    // 约 5µs:小 buffer 的 enqueue 路径 +56%、小 buffer 整条回路 +6%(大 buffer 因传输
    // 占主导只有 1~3%)。对小张量流水线这不可忽略,所以做成编译期开关而非默认开。
    // 想看 GPU 时间:编译时 -DLMFLOW_OCL_PROFILING=1,再用 Image::gpu_duration_ns()。
    const cl_command_queue_properties queue_properties =
        LMFLOW_OCL_PROFILING ? CL_QUEUE_PROFILING_ENABLE : 0;
    queue_ = clCreateCommandQueue(context_, device_, queue_properties, &status);
    Check(status, "clCreateCommandQueue");

    cl_bool unified = CL_FALSE;
    if (clGetDeviceInfo(device_, CL_DEVICE_HOST_UNIFIED_MEMORY, sizeof unified, &unified,
                        nullptr) != CL_SUCCESS) {
      unified = CL_FALSE;  // 读不到就按「非统一」处理:退回拷贝路径永远是安全的那一侧
    }
    host_unified_ = unified == CL_TRUE;
  }

  static std::string BuildLog(cl_program program) {
    cl_device_id device = nullptr;
    if (clGetProgramInfo(program, CL_PROGRAM_DEVICES, sizeof device, &device, nullptr) !=
        CL_SUCCESS) {
      return "<no build log>";
    }
    size_t length = 0;
    clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, 0, nullptr, &length);
    std::string log(length, '\0');
    clGetProgramBuildInfo(program, device, CL_PROGRAM_BUILD_LOG, length, log.data(), nullptr);
    return log;
  }

  cl_platform_id platform_ = nullptr;
  cl_device_id device_ = nullptr;
  cl_context context_ = nullptr;
  cl_command_queue queue_ = nullptr;
  bool host_unified_ = false;
  std::mutex enqueue_mutex_;
  std::mutex pool_mutex_;
  /// 空闲计算 buffer(Image)池(受 pool_mutex_ 保护)。上限见 kMaxPooledSlots /
  /// kMaxPooledBytes;acquire/recycle 见 TryAcquireImageLocked / RecycleImageLocked。
  std::vector<ImageSlot> image_pool_;
  size_t pool_bytes_ = 0;
  uint64_t pool_allocations_ = 0;
  uint64_t pool_reuses_ = 0;
  std::mutex cache_mutex_;
  std::unordered_map<std::string, cl_program> programs_;
  std::unordered_map<std::string, cl_kernel> kernels_;
};

/// 驻留设备的图像负载。
///
/// 这是 GPU 段之间流动的 payload 类型。它**不是** LMFlowBuffer —— 于是端口类型检查
/// 会在建图期拒绝把它接到 CPU 算子上。
///
/// `ready` 是生产者的同步点:消费者应把它放进 wait list(设备侧等待,CPU 不阻塞),
/// 只有 Download 才做主机侧等待。
///
/// 移动语义,不可复制 —— 一个 Image 唯一拥有其 cl_mem。扇出时引擎按引用共享同一个
/// payload(只读),不会复制,所以「参考帧喂给多个消费者」是零拷贝的。
class Image {
 public:
  Image() = default;

  Image(std::shared_ptr<Context> context, cl_mem mem, cl_event ready, int32_t dtype, int ndim,
        const int64_t* shape)
      : context_(std::move(context)), mem_(mem), ready_(ready), dtype_(dtype), ndim_(ndim) {
    for (int i = 0; i < ndim && i < kMaxNdim; ++i) shape_[i] = shape[i];
  }

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

  /// 按元素数与 dtype 在设备上分配一块新缓冲。
  ///
  /// **统一内存设备上会自动追加 `CL_MEM_ALLOC_HOST_PTR`**,让驱动在主机可访问的存储上
  /// 分配 —— 这是 `DownloadMapped` 能把映射直接交给引擎、省掉整次回读拷贝的前提。
  /// 不用 `CL_MEM_USE_HOST_PTR`:它在 Mali 上若对齐/缓存要求不满足会**静默复制**。
  /// 独显上**绝不追加**:那会把 compute buffer 钉在 pinned host memory,GPU 每次访存
  /// 都过 PCIe,比一次回读拷贝糟得多。调用方自己传了 host-ptr 类 flag 时按调用方的来。
  static Image Allocate(const std::shared_ptr<Context>& context, int32_t dtype, int ndim,
                        const int64_t* shape, cl_mem_flags flags = CL_MEM_READ_WRITE) {
    if (ndim <= 0 || ndim > kMaxNdim) {
      throw std::invalid_argument("flow/ocl: Image ndim must be within [1, 4]");
    }
    size_t count = 1;
    for (int i = 0; i < ndim; ++i) {
      if (shape[i] <= 0) throw std::invalid_argument("flow/ocl: Image shape must be positive");
      count *= static_cast<size_t>(shape[i]);
    }
    cl_mem_flags effective = flags;
    const bool caller_chose_host_ptr =
        (flags & (CL_MEM_ALLOC_HOST_PTR | CL_MEM_USE_HOST_PTR)) != 0;
    if (context->host_unified() && !caller_chose_host_ptr) {
      effective |= CL_MEM_ALLOC_HOST_PTR;
    }
    const bool want_host_mapped = (effective & CL_MEM_ALLOC_HOST_PTR) != 0;

    // 计算 buffer 池化:先尝试从池里 best-fit 复用一块空闲 buffer。复用**不改变任何语义**:
    // host_mapped 标志按匹配严格一致(统一内存才有,不能与普通 buffer 混池),ready_ 为 null
    // 等待新生产者。队列是 in-order 的,新生产者的入队必然排在旧消费者命令之后,故复用时
    // 旧命令必然已经(或将要按序)完成 —— 这正是阶段 0 注释里「归还晚于同步点」的前提。
    const size_t bytes = count * DtypeSize(dtype);
    Context::ImageSlot slot;
    bool from_pool = false;
    {
      std::lock_guard<std::mutex> guard(context->pool_mutex());
      from_pool = context->TryAcquireImageLocked(bytes, effective, &slot);
    }
    if (!from_pool) {
      cl_int status = CL_SUCCESS;
      slot.mem = clCreateBuffer(context->context(), effective, bytes, nullptr, &status);
      Check(status, "clCreateBuffer");
      slot.capacity = bytes;
      slot.flags = effective;
      std::lock_guard<std::mutex> guard(context->pool_mutex());
      context->NoteImageAllocationLocked();
    }
    Image image(context, slot.mem, nullptr, dtype, ndim, shape);
    image.host_mapped_ = want_host_mapped;
    image.capacity_ = slot.capacity;
    image.flags_ = slot.flags;
    return image;
  }

  bool valid() const { return mem_ != nullptr; }
  const std::shared_ptr<Context>& context() const { return context_; }
  cl_mem mem() const { return mem_; }
  /// 本 buffer 是否以 CL_MEM_ALLOC_HOST_PTR 分配(即可走零拷贝下载)。
  bool host_mapped() const { return host_mapped_; }
  /// 生产者同步点;可能为 null(表示无需等待)。
  cl_event ready() const { return ready_; }

  /// 生产这个 Image 的那次 GPU 命令的**执行**耗时(纳秒)。
  ///
  /// 用来把「CPU 入队」和「GPU 执行」分开 —— 引擎的节点耗时是在主机侧测的,设备上花的
  /// 时间它看不见。需要编译时 -DLMFLOW_OCL_PROFILING=1(默认关,因为开着每次 dispatch
  /// 多约 5µs)。⚠ 只有事件**已完成**时才可读(否则驱动返回
  /// CL_PROFILING_INFO_NOT_AVAILABLE):先 `clWaitForEvents(1, &image.ready())` 或在
  /// `Download` 之后再问。未开开关 / 无同步点 / 读不到时返回 0,不抛。
  uint64_t gpu_duration_ns() const {
    if (!ready_) return 0;
    cl_ulong start = 0;
    cl_ulong end = 0;
    if (clGetEventProfilingInfo(ready_, CL_PROFILING_COMMAND_START, sizeof start, &start,
                                nullptr) != CL_SUCCESS ||
        clGetEventProfilingInfo(ready_, CL_PROFILING_COMMAND_END, sizeof end, &end, nullptr) !=
            CL_SUCCESS) {
      return 0;
    }
    return end >= start ? static_cast<uint64_t>(end - start) : 0;
  }
  int32_t dtype() const { return dtype_; }
  int ndim() const { return ndim_; }
  int64_t shape(int index) const { return shape_[index]; }

  size_t element_count() const {
    size_t count = 1;
    for (int i = 0; i < ndim_; ++i) count *= static_cast<size_t>(shape_[i]);
    return count;
  }
  size_t byte_size() const { return element_count() * DtypeSize(dtype_); }

  /// 记录本 Image 的生产者同步点(接管一次引用)。
  void SetReady(cl_event event) {
    if (ready_) clReleaseEvent(ready_);
    ready_ = event;
  }

 private:
  void Reset() {
    // clReleaseEvent 在命令仍在飞行时调用是安全的:OpenCL 对事件引用计数,已入队的命令
    // 自己持有引用。
    if (ready_) clReleaseEvent(ready_);
    // 计算 buffer 池化:把 mem **归还池复用**而非 clReleaseMemObject。阶段 0 注释说「池化
    // 需要归还晚于同步点完成」—— 本 adapter 的队列是 in-order 的单一队列:归还的 buffer
    // 被新 Image 复用时,新生产者的入队必然排在所有旧消费者命令之后,于是该前提由队列排序
    // 天然保证,无需主机等待。池满则照旧释放。
    if (mem_ && context_) {
      Context::ImageSlot slot;
      slot.mem = mem_;
      slot.capacity = capacity_;  // 真实创建大小,**不是** byte_size()(见 ImageSlot 注释)
      slot.flags = flags_;
      std::lock_guard<std::mutex> guard(context_->pool_mutex());
      context_->RecycleImageLocked(slot);
      mem_ = nullptr;
    }
    ready_ = nullptr;
    context_.reset();
  }

  void MoveFrom(Image&& other) noexcept {
    context_ = std::move(other.context_);
    mem_ = other.mem_;
    ready_ = other.ready_;
    dtype_ = other.dtype_;
    ndim_ = other.ndim_;
    host_mapped_ = other.host_mapped_;
    capacity_ = other.capacity_;
    flags_ = other.flags_;
    for (int i = 0; i < kMaxNdim; ++i) shape_[i] = other.shape_[i];
    other.mem_ = nullptr;
    other.ready_ = nullptr;
    other.capacity_ = 0;
    other.flags_ = 0;
    other.ndim_ = 0;
  }

  std::shared_ptr<Context> context_;
  cl_mem mem_ = nullptr;
  cl_event ready_ = nullptr;
  int32_t dtype_ = 0;
  int ndim_ = 0;
  bool host_mapped_ = false;
  /// 本 buffer 的真实创建字节数与生效 flags —— 归还池时按它们记录,复用据此匹配。
  size_t capacity_ = 0;
  cl_mem_flags flags_ = 0;
  int64_t shape_[kMaxNdim] = {0, 0, 0, 0};
};

/// 把一个 1~4 维 LMFlowBuffer 上传到设备。
///
/// 阻塞式写:主机指针的生命周期只在本次调用内有保证(输入包随即可能被释放),
/// 非阻塞写会读到已释放的内存。这里正是 §CPU→GPU 边界,阻塞是预期的。
inline Image Upload(const std::shared_ptr<Context>& context, const LMFlowBuffer& buffer) {
  if (buffer.ndim <= 0 || buffer.ndim > kMaxNdim) {
    throw std::invalid_argument("flow/ocl: Upload supports buffers with ndim within [1, 4]");
  }
  if (!BufferIsContiguous(buffer)) {
    throw std::invalid_argument(
        "flow/ocl: Upload needs a row-major contiguous buffer, but the descriptor is strided "
        "(a padded cv::Mat row or a sliced numpy view looks like this); pack it first");
  }
  Image image = Image::Allocate(context, buffer.dtype, buffer.ndim, buffer.shape);
  Check(clEnqueueWriteBuffer(context->queue(), image.mem(), CL_TRUE, 0, image.byte_size(),
                             buffer.data, 0, nullptr, nullptr),
        "clEnqueueWriteBuffer");
  return image;
}

/// 把设备图像下载成一个引擎分配的 LMFlowBuffer 包。
///
/// 阻塞式读,并把生产者的同步点放进 wait list —— 这是整条链上唯一需要主机等待的地方。
inline Packet Download(const Image& image) {
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  for (int i = 0; i < image.ndim(); ++i) shape[i] = image.shape(i);
  // 免初始化分配:回读会把整块写满 —— 输出包按同一 ndim/shape/dtype 分配,其字节数就等于
  // image.byte_size(),而下面的 clEnqueueReadBuffer 正好读这么多字节,故满足「emit 前写满」
  // 的契约。用保证清零的 new_buffer 等于每帧白做一次全缓冲 memset。
  LMFlowBuffer out{};
  Packet packet = Packet::NewBufferUninitialized(image.ndim(), shape, image.dtype(), &out);
  if (packet.IsEmpty()) {
    throw std::runtime_error("flow/ocl: lmflow_packet_new_buffer_uninit failed");
  }
  cl_event wait = image.ready();
  Check(clEnqueueReadBuffer(image.context()->queue(), image.mem(), CL_TRUE, 0, image.byte_size(),
                            out.data, wait ? 1 : 0, wait ? &wait : nullptr, nullptr),
        "clEnqueueReadBuffer");
  return packet;
}

namespace detail {

/// 零拷贝下载交给引擎的释放状态。
///
/// 输出包**可能比产生它的 Image 活得久**(扇出、下游缓存),所以这里连**持有 Image 的那个
/// 输入包**一起留住:它析构才会 clReleaseMemObject。unmap 必须先于那次析构,故两件事在
/// 同一个回调里按序做。
struct MappedHandoff {
  std::shared_ptr<Context> context;
  cl_mem mem = nullptr;
  void* mapped = nullptr;
  Packet owner;
};

/// 引擎在最后一个包引用消失时调用一次;可能落在任意工作线程上。
inline void ReleaseMappedHandoff(void* user_data) {
  auto* state = static_cast<MappedHandoff*>(user_data);
  if (state->context && state->mem && state->mapped) {
    // 不等它完成。注意这里的安全性依据在 buffer 池化之后**变了**:引用计数只保证 cl_mem
    // 对象还活着,不保证没人去**写**它 —— 而 owner 析构时这块 buffer 是归还池、随后可能被
    // 一个新 Image 拿去当输出。真正的保证是单一 in-order 队列的排序:unmap 在此处先入队,
    // 新生产者的入队必然排在它之后,故 unmap 必然先执行完。见 Image::Reset。
    clEnqueueUnmapMemObject(state->context->queue(), state->mem, state->mapped, 0, nullptr,
                            nullptr);
  }
  delete state;  // owner 在此析构 —— cl_mem 归还池、cl_event 释放
}

}  // namespace detail

/// 本 Image 能否走零拷贝下载。见 `DownloadMapped` 的条件说明。
inline bool CanDownloadMapped(const Image& image) {
  return image.valid() && image.host_mapped();
}

/// 零拷贝下载:把已映射的设备内存**直接交给引擎**,不再拷进一个新包。
///
/// 前提是 buffer 以 `CL_MEM_ALLOC_HOST_PTR` 分配 —— 只有统一内存设备会这么分配
/// (见 `Image::Allocate`),用 `CanDownloadMapped` 先问。独显上不适用:那里的 compute
/// buffer 是纯设备内存,`Download()` 的回读拷贝才是对的。
///
/// `image_packet` 是持有 ocl::Image 的包,并在此**移交所有权**:引擎会一直持有它,直到
/// 下游放掉最后一个引用,才 unmap 并释放 cl_mem。
///
/// 交出的视图标了 READONLY —— 它是设备内存的视图,扇出时多个下游共享同一份;想就地改写
/// 的下游会照常走 CoW 复制一份,不会写穿到设备内存上。
inline Packet DownloadMapped(Packet image_packet) {
  const Image* image = image_packet.TryGet<Image>();
  if (!image || !image->valid()) {
    throw std::invalid_argument("flow/ocl: DownloadMapped needs a valid ocl::Image packet");
  }
  if (!image->host_mapped()) {
    throw std::invalid_argument(
        "flow/ocl: DownloadMapped needs a buffer allocated with CL_MEM_ALLOC_HOST_PTR, which "
        "only happens on unified-memory devices; check CanDownloadMapped() first and fall back "
        "to Download()");
  }

  // 先把要用的东西取成值,后面 image_packet 会被移走,不再依赖 image 指针。
  std::shared_ptr<Context> context = image->context();
  cl_mem mem = image->mem();
  const int ndim = image->ndim();
  const int32_t dtype = image->dtype();
  const size_t bytes = image->byte_size();
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  for (int i = 0; i < ndim; ++i) shape[i] = image->shape(i);
  cl_event wait = image->ready();

  // 阻塞映射,并把生产者事件放进 wait list:返回时 GPU 已写完、主机可读。
  // 这同时就是本条链上唯一的主机侧同步点,与 Download() 的语义一致。
  cl_int status = CL_SUCCESS;
  void* mapped = clEnqueueMapBuffer(context->queue(), mem, CL_TRUE, CL_MAP_READ, 0, bytes,
                                    wait ? 1 : 0, wait ? &wait : nullptr, nullptr, &status);
  Check(status, "clEnqueueMapBuffer");

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

  auto* state = new detail::MappedHandoff{context, mem, mapped, std::move(image_packet)};
  Packet adopted = Packet::AdoptBuffer(view, detail::ReleaseMappedHandoff, state);
  if (adopted.IsEmpty()) {
    // 契约:失败时 release_fn 不会被调用,所有权仍归我们,自己收拾干净。
    clEnqueueUnmapMemObject(context->queue(), mem, mapped, 0, nullptr, nullptr);
    state->mapped = nullptr;
    delete state;
    throw std::runtime_error("flow/ocl: lmflow_packet_adopt_buffer rejected the mapped view");
  }
  return adopted;
}

/// 把一个一维 NDRange kernel 入队,产出新的设备图像。
///
/// 生产者的同步点进 wait list、新的 event 记到输出上 —— **CPU 线程不阻塞**,
/// 这就是连续 GPU 段不落主机的机制。`set_args` 负责除输入输出之外的其余参数,
/// 参数序号从 2 开始(0 = 输入 mem,1 = 输出 mem)。
/// 一次 1 输入 1 输出 dispatch 里**因算子而异**的部分。
///
/// 各算子之间真正不同的只有两样:输出的形状/类型,以及工作规模 —— 其余(加锁、把 src/dst
/// 绑到 arg 0/1、设其余参数、把生产者 event 放进 wait list、记录同步点)完全一致。把这两样
/// 参数化,算子里就不必再自己抄一遍入队流程。
///
/// 全部留空即"输出与输入同形同类型、按输出元素数铺 1 维",也就是 `EnqueueUnary` 的行为。
struct DispatchSpec {
  int32_t dtype = 0;                   ///< 0 = 沿用输入的 dtype(cast 类算子改这里)
  int ndim = 0;                        ///< 0 = 沿用输入的 ndim/shape
  const int64_t* shape = nullptr;      ///< ndim 非 0 时必填(resize 类算子改这里)
  int work_dim = 0;                    ///< 0 = 1 维;可为 1/2/3
  size_t global[3] = {0, 0, 0};        ///< work_dim 为 0 时忽略,按输出元素数铺
};

/// 把一个 kernel 入队,产出新的设备图像。
///
/// `set_args` 负责除输入输出之外的参数,序号从 2 开始(0 = 输入 mem,1 = 输出 mem)。
/// 生产者的同步点进 wait list、新 event 记到输出上 —— **CPU 线程不阻塞**,这就是连续
/// GPU 段不落主机的机制。
///
/// 只覆盖「1 输入 1 输出」。多输入算子要自己绑更多 arg,不适用本函数。
template <typename SetArgs>
inline Image Enqueue(const Image& input, const DispatchSpec& spec, const std::string& source,
                     const std::string& entry, SetArgs&& set_args) {
  const std::shared_ptr<Context>& context = input.context();
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  const int out_ndim = spec.ndim != 0 ? spec.ndim : input.ndim();
  if (spec.ndim != 0) {
    if (spec.shape == nullptr) {
      throw std::invalid_argument("flow/ocl: DispatchSpec.ndim set but shape is null");
    }
    if (spec.ndim < 0 || spec.ndim > kMaxNdim) {
      throw std::invalid_argument("flow/ocl: DispatchSpec.ndim must be within [1, 4]");
    }
    for (int i = 0; i < spec.ndim; ++i) shape[i] = spec.shape[i];
  } else {
    for (int i = 0; i < input.ndim(); ++i) shape[i] = input.shape(i);
  }
  Image output = Image::Allocate(context, spec.dtype != 0 ? spec.dtype : input.dtype(), out_ndim,
                                 shape);

  cl_kernel kernel = context->KernelFor(source, entry);
  // 默认按**输出**的元素数铺 —— 写的是输出,输入可能更大或更小(resize 两种都会遇到)。
  const size_t count = output.element_count();
  const int work_dim = spec.work_dim != 0 ? spec.work_dim : 1;
  size_t global[3] = {count, 1, 1};
  if (spec.work_dim != 0) {
    for (int i = 0; i < work_dim; ++i) global[i] = spec.global[i];
  }

  cl_event done = nullptr;
  {
    // clSetKernelArg 对同一 cl_kernel 非线程安全,故设参与入队同锁保护。
    std::lock_guard<std::mutex> guard(context->enqueue_mutex());
    cl_mem in_mem = input.mem();
    cl_mem out_mem = output.mem();
    Check(clSetKernelArg(kernel, 0, sizeof in_mem, &in_mem), "clSetKernelArg(0)");
    Check(clSetKernelArg(kernel, 1, sizeof out_mem, &out_mem), "clSetKernelArg(1)");
    set_args(kernel);
    cl_event wait = input.ready();
    Check(clEnqueueNDRangeKernel(context->queue(), kernel, work_dim, nullptr, global, nullptr,
                                 wait ? 1 : 0, wait ? &wait : nullptr, &done),
          "clEnqueueNDRangeKernel");
  }
  output.SetReady(done);
  return output;
}

/// `Enqueue` 的薄封装:输出与输入同形同类型,按元素数铺 1 维。
template <typename SetArgs>
inline Image EnqueueUnary(const Image& input, const std::string& source, const std::string& entry,
                          SetArgs&& set_args) {
  return Enqueue(input, DispatchSpec{}, source, entry, std::forward<SetArgs>(set_args));
}

/// LMFlowBuffer -> Image。注册名建议 "OclUpload"。
class UploadKernel : public Kernel {
 public:
  static void GetContract(Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
    c.OutputSet<Image>(0);
  }

  Status Process(lmflow::Context& cc) override {
    Packet input = cc.TakeInput(0);
    LMFlowBuffer buffer{};
    if (!input.AsBuffer(&buffer)) return cc.Fail("OclUpload expects an LMFlowBuffer input");
    cc.Emit(0, Packet::Make<Image>(Upload(Context::Shared(), buffer)));
    return Status::Ok();
  }
};

/// Image -> LMFlowBuffer。注册名建议 "OclDownload"。
class DownloadKernel : public Kernel {
 public:
  static void GetContract(Contract& c) {
    c.InputSet<Image>(0);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_BUFFER);
  }

  Status Process(lmflow::Context& cc) override {
    Packet input = cc.TakeInput(0);
    const Image* image = input.TryGet<Image>();
    if (!image || !image->valid()) return cc.Fail("OclDownload expects an ocl::Image input");
    // 统一内存上把映射直接交给引擎,省掉整次回读拷贝;独显照常拷。
    if (CanDownloadMapped(*image)) {
      cc.Emit(0, DownloadMapped(std::move(input)));
    } else {
      cc.Emit(0, Download(*image));
    }
    return Status::Ok();
  }
};

}  // namespace ocl
}  // namespace lmflow

LMFLOW_DECLARE_TYPE_NAME(lmflow::ocl::Image, "lmflow.opencl.Image")

#endif  // LMFLOW_OPENCL_HPP_
