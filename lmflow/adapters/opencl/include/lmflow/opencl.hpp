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

  /// 队列默认 in-order,且 clSetKernelArg 对同一 cl_kernel 不是线程安全的 ——
  /// 故「设参 + 入队」整段由这把锁保护。GPU 算子跑在普通线程池上,可能并发进入。
  std::mutex& enqueue_mutex() { return enqueue_mutex_; }

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
    queue_ = clCreateCommandQueue(context_, device_, 0, &status);
    Check(status, "clCreateCommandQueue");
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
  std::mutex enqueue_mutex_;
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
  /// `flags` 默认 CL_MEM_READ_WRITE。ARM 统一内存上若要主机零拷贝访问,应改用
  /// CL_MEM_ALLOC_HOST_PTR 并配合 clEnqueueMapBuffer —— 注意 CL_MEM_USE_HOST_PTR
  /// 在 Mali 上若对齐/缓存要求不满足会**静默复制**。
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
    cl_int status = CL_SUCCESS;
    cl_mem mem =
        clCreateBuffer(context->context(), flags, count * DtypeSize(dtype), nullptr, &status);
    Check(status, "clCreateBuffer");
    return Image(context, mem, nullptr, dtype, ndim, shape);
  }

  bool valid() const { return mem_ != nullptr; }
  const std::shared_ptr<Context>& context() const { return context_; }
  cl_mem mem() const { return mem_; }
  /// 生产者同步点;可能为 null(表示无需等待)。
  cl_event ready() const { return ready_; }
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
    // clReleaseMemObject / clReleaseEvent 在命令仍在飞行时调用是安全的:
    // OpenCL 对二者引用计数,已入队的命令自己持有引用。因此这里不需要先等待,
    // 也因此阶段 0 不做设备内存池 —— 池化才需要「归还必须晚于同步点完成」。
    if (ready_) clReleaseEvent(ready_);
    if (mem_) clReleaseMemObject(mem_);
    ready_ = nullptr;
    mem_ = nullptr;
    context_.reset();
  }

  void MoveFrom(Image&& other) noexcept {
    context_ = std::move(other.context_);
    mem_ = other.mem_;
    ready_ = other.ready_;
    dtype_ = other.dtype_;
    ndim_ = other.ndim_;
    for (int i = 0; i < kMaxNdim; ++i) shape_[i] = other.shape_[i];
    other.mem_ = nullptr;
    other.ready_ = nullptr;
    other.ndim_ = 0;
  }

  std::shared_ptr<Context> context_;
  cl_mem mem_ = nullptr;
  cl_event ready_ = nullptr;
  int32_t dtype_ = 0;
  int ndim_ = 0;
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

/// 把一个一维 NDRange kernel 入队,产出新的设备图像。
///
/// 生产者的同步点进 wait list、新的 event 记到输出上 —— **CPU 线程不阻塞**,
/// 这就是连续 GPU 段不落主机的机制。`set_args` 负责除输入输出之外的其余参数,
/// 参数序号从 2 开始(0 = 输入 mem,1 = 输出 mem)。
template <typename SetArgs>
inline Image EnqueueUnary(const Image& input, const std::string& source, const std::string& entry,
                          SetArgs&& set_args) {
  const std::shared_ptr<Context>& context = input.context();
  int64_t shape[kMaxNdim] = {0, 0, 0, 0};
  for (int i = 0; i < input.ndim(); ++i) shape[i] = input.shape(i);
  Image output = Image::Allocate(context, input.dtype(), input.ndim(), shape);

  cl_kernel kernel = context->KernelFor(source, entry);
  const size_t count = input.element_count();
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
    Check(clEnqueueNDRangeKernel(context->queue(), kernel, 1, nullptr, &count, nullptr,
                                 wait ? 1 : 0, wait ? &wait : nullptr, &done),
          "clEnqueueNDRangeKernel");
  }
  output.SetReady(done);
  return output;
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
    cc.Emit(0, Download(*image));
    return Status::Ok();
  }
};

}  // namespace ocl
}  // namespace lmflow

LMFLOW_DECLARE_TYPE_NAME(lmflow::ocl::Image, "lmflow.opencl.Image")

#endif  // LMFLOW_OPENCL_HPP_
