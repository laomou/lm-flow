/*
 * flow.hpp — C++ header-only convenience layer (non-ABI)
 *
 * 本层 100% 建立在 flow.h 的 C ABI 之上,自己不碰引擎内部,零运行开销。
 * 作用:提供 lmflow::Graph/Input/Poller/Packet 的 RAII 宿主 API,以及让 C++ 算子作者用
 * 「继承 lmflow::Kernel + override Process + LMFLOW_REGISTER_KERNEL」的方式写算子,
 * 而不必手写函数指针 vtable。C++ 模板便利(Packet::Make<T> 等)全部在本层 monomorphize,
 * 不越过 FFI 边界。不想用它,直接对着 flow.h 裸写宿主或算子亦可。
 */
#ifndef LMFLOW_HPP_
#define LMFLOW_HPP_

#include <cassert>
#include <cstdint>
#include <cstdio>
#include <exception>
#include <optional>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <typeinfo>
#include <utility>
#include <vector>

#include "flow.h"

namespace lmflow {

/* ---------- 类型标识 ----------
 * 用 FNV-1a 哈希 typeid(T).name()(修饰名),**而不是** typeid(T).hash_code()。
 * 原因:hash_code 是实现定义的,不保证跨动态库/跨编译单元一致;而修饰名在同一
 * 平台 ABI 内是稳定字符串。本项目里 C++ 算子编在 core、Python 绑定在另一个 .so,
 * 天然就是跨产物场景,所以从一开始就用稳定方案 —— 事后再改需要全量重编。
 *
 * 但**跨编译器就不保证了**,而且分歧比「修饰方案不同」更彻底:Itanium ABI(GCC/Clang)
 * 下 typeid(int).name() 是 "i",而 **MSVC 的 type_info::name() 返回的是未修饰的可读名** ——
 * "int"、"struct Foo"(修饰形式在另一个 raw_name() 上)。所以两边压根不是同一种命名方案,
 * FNV("i") 与 FNV("int") 毫不相干。
 * 需要跨工具链稳定时,**必须**用 LMFLOW_DECLARE_TYPE_NAME(T, "your.stable.name") 显式指定。
 * cpp/tests/flow_hpp_test.cc 分别钉住 Itanium 与 MSVC 的默认名称和值,防止任一 ABI
 * 内的身份规则静默变化;两套默认值彼此不同是预期行为。 */
constexpr uint64_t Fnv1a(const char* s) {
  uint64_t h = 14695981039346656037ULL;
  while (*s) {
    h ^= static_cast<uint64_t>(static_cast<unsigned char>(*s++));
    h *= 1099511628211ULL;
  }
  return h;
}

/// 内建类型占用 0..15,自定义类型标识必须避开这一段。
/// `constexpr`(而非仅 `inline`):这样 `cpp/abi_assert.cc` 能用 `static_assert`
/// 在**编译期**把「与 Rust 侧 `fnv1a_type_id` 同算法」这条钉死。
constexpr uint64_t NormalizeTypeId(uint64_t h) { return h < 16 ? h + 16 : h; }

template <typename T>
inline uint64_t TypeId() {
  static const uint64_t id = NormalizeTypeId(Fnv1a(typeid(T).name()));
  return id;
}

template <typename T>
inline const char* TypeName() {
  return typeid(T).name();
}

template <typename T>
inline LMFlowStatus RegisterType() {
  static const LMFlowStatus status =
      lmflow_register_type_descriptor(TypeId<T>(), TypeName<T>(), sizeof(T), alignof(T));
  return status;
}

/* ---------- LMFlowBuffer 布局工具 ----------
 * `LMFlowBuffer` 的 strides 以**字节**计,且明确允许非连续布局(见 flow.h)。凡是按
 * 「一整块连续内存」处理 buffer 的代码(bulk memcpy、上传到设备、交给只认连续布局的库),
 * 都必须先问一句是否连续 —— 否则带行填充的 cv::Mat、numpy 切片视图会被**静默读错**。 */

/// 元素总数(各维之积)。
inline int64_t BufferElementCount(const LMFlowBuffer& buffer) {
  int64_t count = 1;
  for (int i = 0; i < buffer.ndim; ++i) count *= buffer.shape[i];
  return count;
}

/// 行优先连续?最内维步长应 = 元素大小,再逐维外推。未知 dtype / 非法 ndim 一律返回 false。
inline bool BufferIsContiguous(const LMFlowBuffer& buffer) {
  const size_t element = lmflow_dtype_size(buffer.dtype);
  if (element == 0 || buffer.ndim <= 0 || buffer.ndim > LMFLOW_MAX_DIMS) return false;
  int64_t expected = static_cast<int64_t>(element);
  for (int i = buffer.ndim - 1; i >= 0; --i) {
    if (buffer.strides[i] != expected) return false;
    expected *= buffer.shape[i];
  }
  return true;
}

/* ---------- Status ---------- */
class Status {
 public:
  Status(LMFlowStatus code) : code_(code) {}  // 允许隐式,便于 return LMFLOW_OK;
  static Status Ok() { return Status(LMFLOW_OK); }
  static Status Error() { return Status(LMFLOW_ERR_KERNEL); }
  bool ok() const { return code_ == LMFLOW_OK; }
  LMFlowStatus code() const { return code_; }

 private:
  LMFlowStatus code_;
};

/* ---------- Packet ----------
 * move-only。own_ 三态精确对应 flow.h 的三种所有权语义:
 *   Local  : Make<T>() 新建、尚未提交 —— 析构直接 drop_fn(防泄漏)
 *   Engine : 从 poller 取得 —— 析构调 lmflow_packet_drop(归还引擎引用)
 *   None   : Borrow 借用引擎输入包,或已 release —— 永不释放 */
class Packet {
 public:
  Packet() : raw_{nullptr, 0, LMFLOW_TS_UNSET, nullptr, nullptr}, own_(Own::None) {}

  template <typename T>
  static Packet Make(T value) {
    if (RegisterType<T>() != LMFLOW_OK) {
      throw std::logic_error(lmflow_last_error());
    }
    Packet p;
    p.raw_.payload = new T(std::move(value));
    p.raw_.type_id = TypeId<T>();
    p.raw_.timestamp = LMFLOW_TS_UNSET;  // 提交时若仍为 UNSET,引擎继承 input_timestamp
    p.raw_.owner = nullptr;
    p.raw_.drop_fn = [](void* q) { delete static_cast<T*>(q); };
    p.own_ = Own::Local;
    return p;
  }

  /* 借用引擎输入包(算子内使用),不获得所有权 */
  static Packet Borrow(LMFlowPacket raw) {
    Packet p;
    p.raw_ = raw;
    p.own_ = Own::None;
    return p;
  }

  /* 接管 lmflow_poller_next 移交的包,析构时归还引擎引用 */
  static Packet Adopt(LMFlowPacket raw) {
    Packet p;
    p.raw_ = raw;
    p.own_ = Own::Engine;
    return p;
  }

  /* 类型安全取值:类型不符返回 nullptr,绝不 UB */
  template <typename T>
  const T* TryGet() const {
    if (!raw_.payload) return nullptr;
    if (raw_.type_id != 0 && raw_.type_id != TypeId<T>()) return nullptr;
    return static_cast<const T*>(raw_.payload);
  }

  template <typename T>
  const T& Get() const {
    const T* p = TryGet<T>();
    assert(p && "lmflow::Packet::Get<T> type mismatch or null packet");
    return *p;
  }

  template <typename T>
  bool Is() const {
    return raw_.payload && raw_.type_id == TypeId<T>();
  }

  bool IsEmpty() const { return raw_.payload == nullptr; }
  uint64_t type_id() const { return raw_.type_id; }
  int64_t Timestamp() const { return raw_.timestamp; }
  /// 设置时间戳。左值返回引用便于链式修改;右值返回值,便于
  /// `cc.Emit(0, Packet::Make<int>(v).At(ts))` 这样直接接到按值传参的接口。
  Packet& At(int64_t ts) & {
    raw_.timestamp = ts;
    return *this;
  }
  Packet At(int64_t ts) && {
    raw_.timestamp = ts;
    return std::move(*this);
  }

  /* ---- 内建 payload 类型(跨语言稳定,见 flow.h)---- */
  static Packet FromBytes(const void* d, size_t n) {
    return Adopt(lmflow_packet_from_bytes(d, n, LMFLOW_TS_UNSET));
  }
  static Packet FromI64(int64_t v) { return Adopt(lmflow_packet_from_i64(v, LMFLOW_TS_UNSET)); }
  static Packet FromF64(double v) { return Adopt(lmflow_packet_from_f64(v, LMFLOW_TS_UNSET)); }
  static Packet FromBool(bool v) { return Adopt(lmflow_packet_from_bool(v, LMFLOW_TS_UNSET)); }
  static Packet FromStr(const char* s) { return Adopt(lmflow_packet_from_str(s, LMFLOW_TS_UNSET)); }
  /// 让**引擎**分配一块连续 N 维缓冲,通过 out 返回可写视图(零拷贝写)。
  /// 典型:BUFFER 算子按输入 shape 造输出。ts 默认 UNSET(提交时继承 input_ts)。
  static Packet NewBuffer(int32_t ndim, const int64_t* shape, int32_t dtype, LMFlowBuffer* out) {
    return Adopt(lmflow_packet_new_buffer(ndim, shape, dtype, LMFLOW_TS_UNSET, out));
  }
  static Packet NewBufferUninitialized(int32_t ndim, const int64_t* shape, int32_t dtype,
                                       LMFlowBuffer* out) {
    return Adopt(
        lmflow_packet_new_buffer_uninit(ndim, shape, dtype, LMFLOW_TS_UNSET, out));
  }
  /// 零拷贝接管外部 CPU buffer。成功后最后一个 Packet 引用释放时调用
  /// release_fn(user_data)；失败时所有权仍归调用方。
  static Packet AdoptBuffer(const LMFlowBuffer& buffer, LMFlowBufferReleaseFn release_fn,
                            void* user_data) {
    return Adopt(
        lmflow_packet_adopt_buffer(&buffer, LMFLOW_TS_UNSET, release_fn, user_data));
  }

  bool AsBytes(const void** d, size_t* n) const { return lmflow_packet_as_bytes(&raw_, d, n); }
  bool AsI64(int64_t* o) const { return lmflow_packet_as_i64(&raw_, o); }
  bool AsF64(double* o) const { return lmflow_packet_as_f64(&raw_, o); }
  bool AsBool(bool* o) const { return lmflow_packet_as_bool(&raw_, o); }
  bool AsStr(const char** o) const { return lmflow_packet_as_str(&raw_, o); }
  LMFlowStatus SetMetadata(const char* key, int64_t value) {
    return lmflow_packet_set_metadata_i64(&raw_, key, value);
  }
  LMFlowStatus SetMetadata(const char* key, double value) {
    return lmflow_packet_set_metadata_f64(&raw_, key, value);
  }
  LMFlowStatus SetMetadata(const char* key, bool value) {
    return lmflow_packet_set_metadata_bool(&raw_, key, value);
  }
  LMFlowStatus SetMetadata(const char* key, const char* value) {
    return lmflow_packet_set_metadata_str(&raw_, key, value);
  }
  bool Metadata(const char* key, int64_t* out) const {
    return lmflow_packet_metadata_i64(&raw_, key, out);
  }
  bool Metadata(const char* key, double* out) const {
    return lmflow_packet_metadata_f64(&raw_, key, out);
  }
  bool Metadata(const char* key, bool* out) const {
    return lmflow_packet_metadata_bool(&raw_, key, out);
  }
  bool Metadata(const char* key, const char** out) const {
    return lmflow_packet_metadata_str(&raw_, key, out);
  }
  bool HasMetadata(const char* key) const { return lmflow_packet_has_metadata(&raw_, key); }
  bool RemoveMetadata(const char* key) { return lmflow_packet_remove_metadata(&raw_, key); }
  std::vector<std::string> MetadataKeys() const {
    std::vector<std::string> keys;
    const size_t count = lmflow_packet_metadata_count(&raw_);
    keys.reserve(count);
    for (size_t index = 0; index < count; ++index) {
      const char* key = lmflow_packet_metadata_key_at(&raw_, index);
      if (key) keys.emplace_back(key);
    }
    return keys;
  }
  /// N 维缓冲的**只读**视图(零拷贝)。cv::Mat 互转见可选 adapter `lmflow/opencv.hpp`。
  bool AsBuffer(LMFlowBuffer* o) const { return lmflow_packet_as_buffer(&raw_, o); }

  /* ---- 引用与写时复制 ---- */
  /// 引用 +1,得到一份自己拥有的包(不拷贝数据)。
  Packet Clone() const { return Adopt(lmflow_packet_clone(&raw_)); }
  /// 取得独占可写视图:独占则零拷贝,被共享才复制。前置条件是本包为自己所拥有
  /// (典型来源是 Context::TakeInput),借用的输入包会返回错误。
  LMFlowStatus MakeMutableBuffer(LMFlowBuffer* o) { return lmflow_packet_make_mutable_buffer(&raw_, o); }
  LMFlowStatus MakeMutableBytes(void** d, size_t* n) {
    return lmflow_packet_make_mutable_bytes(&raw_, d, n);
  }

  LMFlowPacket release() {  // 交给引擎:此后本对象不再释放
    own_ = Own::None;
    return raw_;
  }

  Packet(Packet&& o) noexcept : raw_(o.raw_), own_(o.own_) {
    o.own_ = Own::None;
    o.raw_.payload = nullptr;
  }
  Packet& operator=(Packet&& o) noexcept {
    if (this != &o) {
      reset();
      raw_ = o.raw_;
      own_ = o.own_;
      o.own_ = Own::None;
      o.raw_.payload = nullptr;
    }
    return *this;
  }
  Packet(const Packet&) = delete;
  Packet& operator=(const Packet&) = delete;
  ~Packet() { reset(); }

 private:
  enum class Own { None, Local, Engine };
  void reset() {
    if (own_ == Own::Local) {
      if (raw_.payload && raw_.drop_fn) raw_.drop_fn(raw_.payload);
    } else if (own_ == Own::Engine) {
      lmflow_packet_drop(&raw_);
    }
    own_ = Own::None;
    raw_.payload = nullptr;
  }
  LMFlowPacket raw_;
  Own own_;
};

/* ---------- Graph host API ----------
 * This is the C++ RAII facade for the Rust Graph/Input/Poller API. Handles are
 * move-only and retain the same lifecycle rules as the underlying C ABI. */
class Input;
class Poller;

enum class PollerOverflow {
  Block = LMFLOW_POLLER_BLOCK,
  DropOldest = LMFLOW_POLLER_DROP_OLDEST,
  DropNewest = LMFLOW_POLLER_DROP_NEWEST,
  Latest = LMFLOW_POLLER_LATEST,
};

struct PollerOptions {
  size_t capacity;
  PollerOverflow overflow;
};

class Graph {
 public:
  Graph() : handle_(nullptr) {
    if (lmflow_abi_version() != LMFLOW_ABI_VERSION) {
      throw std::runtime_error("lmflow ABI mismatch: engine vs header");
    }
    handle_ = lmflow_graph_new();
    if (!handle_) throw std::runtime_error(lmflow_last_error());
  }
  ~Graph() { destroy(); }

  Graph(const Graph&) = delete;
  Graph& operator=(const Graph&) = delete;
  Graph(Graph&& other) noexcept : handle_(other.handle_) { other.handle_ = nullptr; }
  Graph& operator=(Graph&& other) noexcept {
    if (this != &other) {
      destroy();
      handle_ = other.handle_;
      other.handle_ = nullptr;
    }
    return *this;
  }

  static Graph from_yaml(const char* yaml) {
    Graph graph;
    const Status status = graph.init_from_yaml(yaml);
    if (!status.ok()) throw std::runtime_error(graph.error_message());
    return graph;
  }

  static Graph from_yaml_file(const char* path) {
    Graph graph;
    const Status status = lmflow_status(lmflow_graph_init_from_yaml_file(graph.handle_, path));
    if (!status.ok()) throw std::runtime_error(graph.error_message());
    return graph;
  }

  bool valid() const { return handle_ != nullptr; }
  Status init_from_yaml(const char* yaml) {
    ensure_handle();
    return lmflow_status(lmflow_graph_init_from_yaml(handle_, yaml));
  }
  Status start() {
    ensure_handle();
    return lmflow_status(lmflow_graph_start(handle_));
  }
  Status reset() {
    ensure_handle();
    return lmflow_status(lmflow_graph_reset(handle_));
  }

  Input input(const char* port);
  Status close_input(const char* port) {
    ensure_handle();
    return lmflow_status(lmflow_graph_close_input(handle_, port));
  }
  void close_all_inputs() {
    ensure_handle();
    lmflow_graph_close_all_inputs(handle_);
  }
  Status set_side_packet(const char* name, Packet packet) {
    ensure_handle();
    return lmflow_status(lmflow_graph_set_side_packet(handle_, name, packet.release()));
  }

  Poller add_poller(const char* port);
  Poller add_poller_ex(const char* port, bool observe_timestamp_bounds);
  Poller add_poller_with_options(const char* port, PollerOptions options);
  Poller add_poller_bounded(const char* port, size_t capacity, int overflow_policy);

  void cancel() {
    ensure_handle();
    lmflow_graph_cancel(handle_);
  }
  Status finish() {
    ensure_handle();
    lmflow_graph_close_all_inputs(handle_);
    return lmflow_status(lmflow_graph_wait_done(handle_));
  }
  Status stop() {
    ensure_handle();
    lmflow_graph_cancel(handle_);
    const Status status = lmflow_status(lmflow_graph_wait_done(handle_));
    return status.code() == LMFLOW_ERR_CANCELLED ? Status::Ok() : status;
  }
  Status wait_done() {
    ensure_handle();
    return lmflow_status(lmflow_graph_wait_done(handle_));
  }
  Status wait_done_timeout(int64_t timeout_ms) {
    ensure_handle();
    return lmflow_status(lmflow_graph_wait_done_timeout(handle_, timeout_ms));
  }
  Status wait_until_idle() {
    ensure_handle();
    return lmflow_status(lmflow_graph_wait_until_idle(handle_));
  }
  Status wait_until_idle_timeout(int64_t timeout_ms) {
    ensure_handle();
    return lmflow_status(lmflow_graph_wait_until_idle_timeout(handle_, timeout_ms));
  }
  bool pump_step() {
    ensure_handle();
    return lmflow_graph_pump_step(handle_);
  }
  void pause() {
    ensure_handle();
    lmflow_graph_pause(handle_);
  }
  void resume() {
    ensure_handle();
    lmflow_graph_resume(handle_);
  }
  LMFlowGraphState state() const {
    ensure_handle();
    return lmflow_graph_state(handle_);
  }
  const char* last_error() const {
    ensure_handle();
    return lmflow_graph_last_error(handle_);
  }

 private:
  const char* error_message() const {
    const char* message = lmflow_graph_last_error(handle_);
    return (message && *message) ? message : lmflow_last_error();
  }
  static Status lmflow_status(LMFlowStatus code) { return Status(code); }
  void ensure_handle() const {
    if (!handle_) throw std::logic_error("lmflow::Graph is empty");
  }
  void destroy() {
    if (handle_) {
      lmflow_graph_free(handle_);
      handle_ = nullptr;
    }
  }
  LMFlowGraph* handle_ = nullptr;
  friend class Input;
  friend class Poller;
};

class Input {
 public:
  Input() = default;
  ~Input() { reset(); }
  Input(const Input&) = delete;
  Input& operator=(const Input&) = delete;
  Input(Input&& other) noexcept : handle_(other.handle_) { other.handle_ = nullptr; }
  Input& operator=(Input&& other) noexcept {
    if (this != &other) {
      reset();
      handle_ = other.handle_;
      other.handle_ = nullptr;
    }
    return *this;
  }
  bool valid() const { return handle_ != nullptr; }
  Status send(Packet packet) {
    ensure_handle();
    return Status(lmflow_input_send(handle_, packet.release()));
  }
  /* Returns false for backpressure. Ownership transfers to the engine before
   * the call returns, so a false result means the packet was dropped. */
  bool try_send(Packet packet) {
    ensure_handle();
    const LMFlowStatus status = lmflow_input_try_send(handle_, packet.release());
    if (status == LMFLOW_OK) return true;
    if (status == LMFLOW_ERR_WOULD_BLOCK) return false;
    throw std::runtime_error(lmflow_last_error());
  }
  void close() {
    ensure_handle();
    lmflow_input_close(handle_);
  }

 private:
  explicit Input(LMFlowInput* handle) : handle_(handle) {}
  void ensure_handle() const {
    if (!handle_) throw std::logic_error("lmflow::Input is empty");
  }
  void reset() {
    if (handle_) {
      lmflow_input_free(handle_);
      handle_ = nullptr;
    }
  }
  LMFlowInput* handle_ = nullptr;
  friend class Graph;
};

class Poller {
 public:
  Poller() = default;
  ~Poller() { reset(); }
  Poller(const Poller&) = delete;
  Poller& operator=(const Poller&) = delete;
  Poller(Poller&& other) noexcept : handle_(other.handle_) { other.handle_ = nullptr; }
  Poller& operator=(Poller&& other) noexcept {
    if (this != &other) {
      reset();
      handle_ = other.handle_;
      other.handle_ = nullptr;
    }
    return *this;
  }
  bool valid() const { return handle_ != nullptr; }
  std::optional<Packet> next() {
    ensure_handle();
    LMFlowPacket raw{};
    if (!lmflow_poller_next(handle_, &raw)) return std::nullopt;
    return Packet::Adopt(raw);
  }
  std::optional<Packet> try_next() {
    ensure_handle();
    LMFlowPacket raw{};
    if (!lmflow_poller_try_next(handle_, &raw)) return std::nullopt;
    return Packet::Adopt(raw);
  }
  /* Returns an empty optional on timeout or closed poller. Other failures
   * throw; successful packets are transferred into the returned Packet. */
  std::optional<Packet> next_timeout(int64_t timeout_ms) {
    ensure_handle();
    LMFlowPacket raw{};
    const LMFlowStatus status = lmflow_poller_next_timeout(handle_, &raw, timeout_ms);
    if (status == LMFLOW_OK) return Packet::Adopt(raw);
    if (status == LMFLOW_ERR_TIMEOUT || status == LMFLOW_ERR_CLOSED) return std::nullopt;
    throw std::runtime_error(lmflow_last_error());
  }
  uint64_t dropped_count() const {
    ensure_handle();
    return lmflow_poller_dropped_count(handle_);
  }

 private:
  explicit Poller(LMFlowPoller* handle) : handle_(handle) {}
  void ensure_handle() const {
    if (!handle_) throw std::logic_error("lmflow::Poller is empty");
  }
  void reset() {
    if (handle_) {
      lmflow_poller_free(handle_);
      handle_ = nullptr;
    }
  }
  LMFlowPoller* handle_ = nullptr;
  friend class Graph;
};

inline Input Graph::input(const char* port) {
  ensure_handle();
  LMFlowInput* handle = lmflow_graph_input(handle_, port);
  if (!handle) throw std::runtime_error(error_message());
  return Input(handle);
}

inline Poller Graph::add_poller(const char* port) {
  ensure_handle();
  LMFlowPoller* handle = lmflow_graph_add_poller(handle_, port);
  if (!handle) throw std::runtime_error(error_message());
  return Poller(handle);
}

inline Poller Graph::add_poller_ex(const char* port, bool observe_timestamp_bounds) {
  ensure_handle();
  LMFlowPoller* handle = lmflow_graph_add_poller_ex(handle_, port, observe_timestamp_bounds);
  if (!handle) throw std::runtime_error(error_message());
  return Poller(handle);
}

inline Poller Graph::add_poller_with_options(const char* port, PollerOptions options) {
  return add_poller_bounded(port, options.capacity, static_cast<int>(options.overflow));
}

inline Poller Graph::add_poller_bounded(
    const char* port, size_t capacity, int overflow_policy) {
  ensure_handle();
  LMFlowPoller* handle =
      lmflow_graph_add_poller_bounded(handle_, port, capacity, overflow_policy);
  if (!handle) throw std::runtime_error(error_message());
  return Poller(handle);
}

/* ---------- Contract:在 GetContract 里声明端口类型 ---------- */
class Contract {
 public:
  explicit Contract(LMFlowContract* c) : c_(c) {}
  Contract(const Contract&) = delete;
  Contract& operator=(const Contract&) = delete;

  size_t NumInputs() const { return lmflow_contract_num_inputs(c_); }
  size_t NumOutputs() const { return lmflow_contract_num_outputs(c_); }
  size_t InputId(const char* tag, size_t index = 0) const { return lmflow_contract_input_id(c_, tag, index); }
  size_t OutputId(const char* tag, size_t index = 0) const { return lmflow_contract_output_id(c_, tag, index); }
  const char* InputName(size_t i) const { return lmflow_contract_input_name(c_, i); }
  const char* OutputName(size_t i) const { return lmflow_contract_output_name(c_, i); }
  size_t InputIndex(const char* name) const { return lmflow_contract_input_index(c_, name); }
  size_t OutputIndex(const char* name) const { return lmflow_contract_output_index(c_, name); }
  /// 声明必需的 side packet;宿主漏注入则 init 阶段报错。
  void RequireSidePacket(const char* name) { lmflow_contract_require_side_packet(c_, name); }

  void InputSetAny(size_t i) { lmflow_contract_input_set_any(c_, i); }
  void OutputSetAny(size_t i) { lmflow_contract_output_set_any(c_, i); }
  template <typename T>
  void InputSet(size_t i) {
    if (RegisterType<T>() != LMFLOW_OK) {
      throw std::logic_error(lmflow_last_error());
    }
    lmflow_contract_input_set_type(c_, i, TypeId<T>());
  }
  template <typename T>
  void OutputSet(size_t i) {
    if (RegisterType<T>() != LMFLOW_OK) {
      throw std::logic_error(lmflow_last_error());
    }
    lmflow_contract_output_set_type(c_, i, TypeId<T>());
  }

  /// 按**内建类型**声明(LMFLOW_TYPE_I64 等)。跨语言算子应当用这个而不是
  /// `InputSet<T>` —— 后者用的是 C++ 的 typeid,Python/Go 侧无从产生同样的标识。
  void InputSetBuiltin(size_t i, uint64_t builtin) {
    lmflow_contract_input_set_type(c_, i, builtin);
  }
  void OutputSetBuiltin(size_t i, uint64_t builtin) {
    lmflow_contract_output_set_type(c_, i, builtin);
  }

 private:
  LMFlowContract* c_;
};

/* ---------- Context(仅回调期有效,故禁止拷贝/移动以防存留)---------- */
class Context {
 public:
  explicit Context(LMFlowContext* c) : c_(c) {}
  Context(const Context&) = delete;
  Context& operator=(const Context&) = delete;
  Context(Context&&) = delete;
  Context& operator=(Context&&) = delete;

  size_t NumInputs() const { return lmflow_ctx_num_inputs(c_); }
  size_t NumOutputs() const { return lmflow_ctx_num_outputs(c_); }

  /* ---- 自我信息 / 日志 / 错误 ---- */
  const char* NodeName() const { return lmflow_ctx_node_name(c_); }
  const char* KernelName() const { return lmflow_ctx_kernel_name(c_); }
  void Log(LMFlowLogLevel level, const char* msg) const { lmflow_ctx_log(c_, level, msg); }
  void LogInfo(const char* msg) const { Log(LMFLOW_LOG_INFO, msg); }
  void LogWarn(const char* msg) const { Log(LMFLOW_LOG_WARN, msg); }
  /// 设置失败原因,随后返回非 0 状态码 —— 否则宿主只拿到错误码、无从诊断。
  void SetError(const char* msg) const { lmflow_ctx_set_error(c_, msg); }
  /// 业务计数器,按名字聚合到图上,宿主可读。
  void CounterAdd(const char* name, int64_t delta = 1) const {
    lmflow_ctx_counter_add(c_, name, delta);
  }
  lmflow::Status Fail(const char* msg) const {
    SetError(msg);
    return Status(LMFLOW_ERR_KERNEL);
  }
  /// close 的触发原因:正常排空 / 图内出错 / 被取消。
  LMFlowCloseReason CloseReason() const { return lmflow_ctx_close_reason(c_); }
  /// 源算子(0 输入)自报「已产完」:引擎停止再触发本节点、关其输出边,图随之终止。
  void SourceDone() const { lmflow_ctx_source_done(c_); }
  /// 源算子协作式让出 worker，并在 delay_ms 后再次触发。
  void SourceYield(uint64_t delay_ms) const { lmflow_ctx_source_yield(c_, delay_ms); }

  /* 按 tag 定位端口,避免依赖 YAML 书写顺序 */
  size_t InputId(const char* tag, size_t index = 0) const { return lmflow_ctx_input_id(c_, tag, index); }
  size_t OutputId(const char* tag, size_t index = 0) const { return lmflow_ctx_output_id(c_, tag, index); }
  const char* InputName(size_t i) const { return lmflow_ctx_input_name(c_, i); }
  const char* OutputName(size_t i) const { return lmflow_ctx_output_name(c_, i); }
  /// 按**边名**取序号(与 InputId 的按 tag 互补);查不到返回 LMFLOW_INVALID_ID
  size_t InputIndex(const char* name) const { return lmflow_ctx_input_index(c_, name); }
  size_t OutputIndex(const char* name) const { return lmflow_ctx_output_index(c_, name); }

  bool InputIsEmpty(size_t i) const { return lmflow_ctx_input_is_empty(c_, i); }
  /// 该口是否已终结(上游已关且排空,永远不会再有数据)。
  bool InputIsDone(size_t i) const { return lmflow_ctx_input_is_done(c_, i); }
  Packet Input(size_t i) const { return Packet::Borrow(lmflow_ctx_input(c_, i)); }
  /// 本次某输入口的包数(单包策略恒 0/1;`batch` 策略为该批大小)。
  size_t InputCount(size_t i) const { return lmflow_ctx_input_count(c_, i); }
  /// 借用某输入口的第 k 个包(`batch` 策略下配合 InputCount 遍历一批)。
  Packet InputAt(size_t i, size_t k) const { return Packet::Borrow(lmflow_ctx_input_at(c_, i, k)); }
  /* 快路径:已知类型时直取指针,省一次结构体按值返回 */
  template <typename T>
  const T* InputPtr(size_t i) const {
    return static_cast<const T*>(lmflow_ctx_input_payload(c_, i));
  }
  int64_t InputTimestamp() const { return lmflow_ctx_input_timestamp(c_); }
  /// **取走**输入包(所有权移交,输入槽变空)。CoW 省拷贝的第一步。
  Packet TakeInput(size_t i) { return Packet::Adopt(lmflow_ctx_take_input(c_, i)); }

  void Emit(size_t i, Packet p) { lmflow_ctx_emit(c_, i, p.release()); }
  void Forward(size_t in, size_t out) { lmflow_ctx_forward(c_, in, out); }
  void SetNextTimestampBound(size_t i, int64_t bound) { lmflow_ctx_set_next_ts_bound(c_, i, bound); }

  /* ---- node options ---- */
  bool HasOption(const char* key) const { return lmflow_ctx_has_option(c_, key); }
  int64_t OptionI64(const char* key, int64_t def = 0) const { return lmflow_ctx_option_i64(c_, key, def); }
  double OptionF64(const char* key, double def = 0.0) const { return lmflow_ctx_option_f64(c_, key, def); }
  bool OptionBool(const char* key, bool def = false) const { return lmflow_ctx_option_bool(c_, key, def); }
  const char* OptionStr(const char* key, const char* def = "") const {
    return lmflow_ctx_option_str(c_, key, def);
  }
  const char* OptionsJson() const { return lmflow_ctx_options_json(c_); }

  /// 必需参数:缺失或类型不符即返回错误(算子应在 Open 里直接失败,
  /// 让配置问题当场暴露,而不是静默走默认值)。
  LMFlowStatus RequireOption(const char* key, int64_t* o) const {
    return lmflow_ctx_require_option_i64(c_, key, o);
  }
  LMFlowStatus RequireOption(const char* key, double* o) const {
    return lmflow_ctx_require_option_f64(c_, key, o);
  }
  LMFlowStatus RequireOption(const char* key, bool* o) const {
    return lmflow_ctx_require_option_bool(c_, key, o);
  }
  LMFlowStatus RequireOption(const char* key, const char** o) const {
    return lmflow_ctx_require_option_str(c_, key, o);
  }

  /// 数组参数。返回实际元素个数(可能大于 cap,此时只写入 cap 个)。
  size_t OptionCount(const char* key) const { return lmflow_ctx_option_count(c_, key); }
  size_t OptionArray(const char* key, int64_t* out, size_t cap) const {
    return lmflow_ctx_option_i64_array(c_, key, out, cap);
  }
  size_t OptionArray(const char* key, double* out, size_t cap) const {
    return lmflow_ctx_option_f64_array(c_, key, out, cap);
  }
  size_t OptionArray(const char* key, const char** out, size_t cap) const {
    return lmflow_ctx_option_str_array(c_, key, out, cap);
  }

  /* ---- side packet:宿主注入的常量对象(模型句柄、标定参数…)---- */
  bool HasSidePacket(const char* name) const { return lmflow_ctx_has_side_packet(c_, name); }
  /// 借用,不得 drop、不得跨回调留存。
  Packet SidePacket(const char* name) const {
    return Packet::Borrow(lmflow_ctx_side_packet(c_, name));
  }

 private:
  LMFlowContext* c_;
};

/* ---------- Kernel 基类 ----------
 * 必须实现 Process。可选实现 Open/Close。
 * 可选提供静态 `static void GetContract(lmflow::Contract&)` 声明端口类型 ——
 * 有则自动接线(SFINAE 检测),无则跳过。 */
class Kernel {
 public:
  virtual ~Kernel() = default;
  virtual Status Open(Context&) { return Status::Ok(); }
  virtual Status Process(Context&) = 0;
  virtual Status Close(Context&) { return Status::Ok(); }
};

namespace internal {

template <typename T, typename = void>
struct HasGetContract : std::false_type {};
template <typename T>
struct HasGetContract<T, std::void_t<decltype(T::GetContract(std::declval<Contract&>()))>>
    : std::true_type {};

}  // namespace internal

/* ---------- 适配器:把虚类桥成 C ABI vtable,并挡住异常穿越 FFI ---------- */
template <typename T>
struct KernelAdapter {
  static_assert(std::is_base_of<Kernel, T>::value, "kernel must inherit lmflow::Kernel");

  static void* create(void*) {
    // 构造函数可能抛(如打开设备失败)—— 绝不能让 C++ 异常穿越 extern "C" 回到
    // Rust 引擎(那是 UB;Rust 的 catch_unwind 也接不住 C++ 异常)。失败返回 nullptr,
    // 随后 open/process 会因 self==nullptr 返回错误,图随即失败(与 Python 端一致)。
    try {
      return new T();
    } catch (...) {
      return nullptr;
    }
  }
  static void destroy(void* self) { delete static_cast<T*>(self); }

  static void get_contract(void*, LMFlowContract* c) {
    if constexpr (internal::HasGetContract<T>::value) {
      try {
        Contract ct(c);
        T::GetContract(ct);
      } catch (const std::exception& e) {
        lmflow_contract_set_error(c, e.what());
      } catch (...) {
        lmflow_contract_set_error(c, "C++ GetContract threw a non-standard exception");
      }
    }
    (void)c;
  }

  static LMFlowStatus open(void* self, LMFlowContext* c) {
    if (!self) return LMFLOW_ERR_KERNEL;  // create 失败(构造抛异常)
    try {
      Context cc(c);
      return static_cast<T*>(self)->Open(cc).code();
    } catch (const std::exception& e) {
      // 把 what() 带给引擎:否则异常这条路只剩一个错误码、诊断信息为零
      //(返回失败 Status 的那条路靠 cc.Fail 已有文本)。
      if (c != nullptr) Context(c).SetError(e.what());
      return LMFLOW_ERR_KERNEL;
    } catch (...) {
      if (c != nullptr) Context(c).SetError("Open threw a non-std exception");
      return LMFLOW_ERR_KERNEL;
    }
  }
  static LMFlowStatus process(void* self, LMFlowContext* c) {
    if (!self) return LMFLOW_ERR_KERNEL;
    try {
      Context cc(c);
      return static_cast<T*>(self)->Process(cc).code();
    } catch (const std::exception& e) {
      // 把 what() 带给引擎:否则异常这条路只剩一个错误码、诊断信息为零
      //(返回失败 Status 的那条路靠 cc.Fail 已有文本)。
      if (c != nullptr) Context(c).SetError(e.what());
      return LMFLOW_ERR_KERNEL;
    } catch (...) {
      if (c != nullptr) Context(c).SetError("Process threw a non-std exception");
      return LMFLOW_ERR_KERNEL;
    }
  }
  static LMFlowStatus close(void* self, LMFlowContext* c) {
    if (!self) return LMFLOW_ERR_KERNEL;
    try {
      Context cc(c);
      return static_cast<T*>(self)->Close(cc).code();
    } catch (const std::exception& e) {
      // 把 what() 带给引擎:否则异常这条路只剩一个错误码、诊断信息为零
      //(返回失败 Status 的那条路靠 cc.Fail 已有文本)。
      if (c != nullptr) Context(c).SetError(e.what());
      return LMFLOW_ERR_KERNEL;
    } catch (...) {
      if (c != nullptr) Context(c).SetError("Close threw a non-std exception");
      return LMFLOW_ERR_KERNEL;
    }
  }

  static const LMFlowKernelVTable* vtable() {
    static const LMFlowKernelVTable vt = {
        &create,
        internal::HasGetContract<T>::value ? &get_contract : nullptr,
        &open,
        &process,
        &close,
        &destroy};
    return &vt;
  }
};

}  // namespace lmflow

/*
 * 注册宏。LMFLOW_REGISTER_KERNEL(T) 用 #T 当注册名(须与 YAML 的 kernel 字段一致);
 * 若类在命名空间内、或想让 YAML 用别名,请用 LMFLOW_REGISTER_KERNEL_AS。
 *
 * 注意:静态初始化在静态库中可能被链接器裁剪。官方 lmflow::lmflow /
 * lmflow::kernels target 会自动保留完整 archive；自定义静态算子库也须使用
 * --whole-archive、-force_load 或 /WHOLEARCHIVE 等平台对应选项。
 */
/*
 * 条件断言:不成立就**带着「哪个表达式、哪个文件哪一行」**返回算子失败。
 *
 * 为什么要传 `cc`:C ABI 跨界只能过一个 `int32_t`(ADR #1),错误**文本**得另走一条
 * 通道 —— 经 `lmflow_ctx_set_error` 存进 Context。所以「返回失败」与「说明原因」在
 * 本框架里是两件事,而直接 `return Status::Error()` 会让引擎只拿到一个码、原因为空。
 * 这两个宏把它们绑成一个动作,让你**难以**漏掉文本:
 *
 *     lmflow::Status Process(lmflow::Context& cc) override {
 *       LMFlowBuffer in{};
 *       LMFLOW_RET_CHECK(cc, cc.Input(0).AsBuffer(&in));      // 自动文本
 *       LMFLOW_RET_CHECK_MSG(cc, in.ndim == 4, "只接受 NCHW"); // 自定义 + 自动位置
 *       ...
 *     }
 *
 * 文本超长会被 snprintf 安全截断。只能用在返回 `lmflow::Status` 的函数里。
 */
#define LMFLOW_RET_CHECK_MSG(cc, cond, msg)                                            \
  do {                                                                                 \
    if (!(cond)) {                                                                     \
      char lmflow_rc_buf_[256];                                                        \
      std::snprintf(lmflow_rc_buf_, sizeof(lmflow_rc_buf_), "%s (check failed: %s at %s:%d)", \
                    (msg), #cond, __FILE__, __LINE__);                                 \
      return (cc).Fail(lmflow_rc_buf_);                                                \
    }                                                                                  \
  } while (0)

#define LMFLOW_RET_CHECK(cc, cond)                                                     \
  do {                                                                                 \
    if (!(cond)) {                                                                     \
      char lmflow_rc_buf_[256];                                                        \
      std::snprintf(lmflow_rc_buf_, sizeof(lmflow_rc_buf_), "check failed: %s (at %s:%d)", \
                    #cond, __FILE__, __LINE__);                                        \
      return (cc).Fail(lmflow_rc_buf_);                                                \
    }                                                                                  \
  } while (0)

#define LMFLOW_REGISTER_KERNEL_AS(T, name_str)                                             \
  namespace {                                                                            \
  struct LMFlowReg_##T {                                                                   \
    LMFlowReg_##T() {                                                                      \
      lmflow_register_kernel_with_language(name_str, lmflow::KernelAdapter<T>::vtable(),   \
                                           nullptr, LMFLOW_KERNEL_LANGUAGE_CPP);            \
    }                                                                                      \
  };                                                                                     \
  static LMFlowReg_##T g_flow_reg_##T;                                                     \
  }

#define LMFLOW_REGISTER_KERNEL(T) LMFLOW_REGISTER_KERNEL_AS(T, #T)

/*
 * 为类型指定**跨工具链稳定**的标识名。需要让不同编译器编出的算子互通时使用。
 * 必须在任何用到该类型的 Packet/Contract 之前出现(通常放在类型定义的头文件里)。
 *
 *   struct MyImage { ... };
 *   LMFLOW_DECLARE_TYPE_NAME(MyImage, "myproj.MyImage")
 */
#define LMFLOW_DECLARE_TYPE_NAME(T, name_str)                                          \
  namespace lmflow {                                                                   \
  template <>                                                                        \
  inline uint64_t TypeId<T>() {                                                      \
    static const uint64_t id = NormalizeTypeId(Fnv1a(name_str));                     \
    return id;                                                                       \
  }                                                                                  \
  template <>                                                                        \
  inline const char* TypeName<T>() {                                                 \
    return name_str;                                                                 \
  }                                                                                  \
  }

#endif  // LMFLOW_HPP_
