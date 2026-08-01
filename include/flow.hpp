/*
 * flow.hpp — 可选的 C++ 算子糖(header-only,非 ABI)
 *
 * 本层 100% 建立在 flow.h 的 C ABI 之上,自己不碰引擎内部,零运行开销。
 * 作用:让 C++ 算子作者用「继承 lmflow::Kernel + override Process + LMFLOW_REGISTER_KERNEL」
 * 的方式写算子,而不必手写函数指针 vtable。C++ 模板便利(Packet::Make<T> 等)全部在
 * 本层 monomorphize,不越过 FFI 边界。不想用它,直接对着 flow.h 裸写算子亦可。
 */
#ifndef LMFLOW_HPP_
#define LMFLOW_HPP_

#include <cassert>
#include <cstdint>
#include <type_traits>
#include <typeinfo>
#include <utility>

#include "flow.h"

namespace lmflow {

/* ---------- 类型标识 ----------
 * 用 FNV-1a 哈希 typeid(T).name()(修饰名),**而不是** typeid(T).hash_code()。
 * 原因:hash_code 是实现定义的,不保证跨动态库/跨编译单元一致;而修饰名在同一
 * 平台 ABI 内是稳定字符串。本项目里 C++ 算子编在 core、Python 绑定在另一个 .so,
 * 天然就是跨产物场景,所以从一开始就用稳定方案 —— 事后再改需要全量重编。
 *
 * 修饰名跨编译器仍可能不同(Itanium ABI 的 GCC/Clang 一致,MSVC 不同)。
 * 需要跨工具链稳定时,用 LMFLOW_DECLARE_TYPE_NAME(T, "your.stable.name") 显式指定。 */
constexpr uint64_t Fnv1a(const char* s) {
  uint64_t h = 14695981039346656037ULL;
  while (*s) {
    h ^= static_cast<uint64_t>(static_cast<unsigned char>(*s++));
    h *= 1099511628211ULL;
  }
  return h;
}

/// 内建类型占用 0..15,自定义类型标识必须避开这一段。
inline uint64_t NormalizeTypeId(uint64_t h) { return h < 16 ? h + 16 : h; }

template <typename T>
inline uint64_t TypeId() {
  static const uint64_t id = NormalizeTypeId(Fnv1a(typeid(T).name()));
  return id;
}

/* ---------- Status ---------- */
class Status {
 public:
  Status(LmflowStatus code) : code_(code) {}  // 允许隐式,便于 return LMFLOW_OK;
  static Status Ok() { return Status(LMFLOW_OK); }
  static Status Error() { return Status(LMFLOW_ERR_KERNEL); }
  bool ok() const { return code_ == LMFLOW_OK; }
  LmflowStatus code() const { return code_; }

 private:
  LmflowStatus code_;
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
  static Packet Borrow(LmflowPacket raw) {
    Packet p;
    p.raw_ = raw;
    p.own_ = Own::None;
    return p;
  }

  /* 接管 lmflow_poller_next 移交的包,析构时归还引擎引用 */
  static Packet Adopt(LmflowPacket raw) {
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
    assert(p && "lmflow::Packet::Get<T> 类型不匹配或空包");
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

  bool AsBytes(const void** d, size_t* n) const { return lmflow_packet_as_bytes(&raw_, d, n); }
  bool AsI64(int64_t* o) const { return lmflow_packet_as_i64(&raw_, o); }
  bool AsF64(double* o) const { return lmflow_packet_as_f64(&raw_, o); }
  bool AsBool(bool* o) const { return lmflow_packet_as_bool(&raw_, o); }
  bool AsStr(const char** o) const { return lmflow_packet_as_str(&raw_, o); }
  /// N 维缓冲的**只读**视图(零拷贝)。cv::Mat 互转见可选头 flow_cv.hpp。
  bool AsBuffer(LmflowBuffer* o) const { return lmflow_packet_as_buffer(&raw_, o); }

  /* ---- 引用与写时复制 ---- */
  /// 引用 +1,得到一份自己拥有的包(不拷贝数据)。
  Packet Clone() const { return Adopt(lmflow_packet_clone(&raw_)); }
  /// 取得独占可写视图:独占则零拷贝,被共享才复制。前置条件是本包为自己所拥有
  /// (典型来源是 Context::TakeInput),借用的输入包会返回错误。
  LmflowStatus MakeMutableBuffer(LmflowBuffer* o) { return lmflow_packet_make_mutable_buffer(&raw_, o); }
  LmflowStatus MakeMutableBytes(void** d, size_t* n) {
    return lmflow_packet_make_mutable_bytes(&raw_, d, n);
  }

  LmflowPacket release() {  // 交给引擎:此后本对象不再释放
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
  LmflowPacket raw_;
  Own own_;
};

/* ---------- Contract:在 GetContract 里声明端口类型 ---------- */
class Contract {
 public:
  explicit Contract(LmflowContract* c) : c_(c) {}
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
    lmflow_contract_input_set_type(c_, i, TypeId<T>());
  }
  template <typename T>
  void OutputSet(size_t i) {
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
  LmflowContract* c_;
};

/* ---------- Context(仅回调期有效,故禁止拷贝/移动以防存留)---------- */
class Context {
 public:
  explicit Context(LmflowContext* c) : c_(c) {}
  Context(const Context&) = delete;
  Context& operator=(const Context&) = delete;
  Context(Context&&) = delete;
  Context& operator=(Context&&) = delete;

  size_t NumInputs() const { return lmflow_ctx_num_inputs(c_); }
  size_t NumOutputs() const { return lmflow_ctx_num_outputs(c_); }

  /* ---- 自我信息 / 日志 / 错误 ---- */
  const char* NodeName() const { return lmflow_ctx_node_name(c_); }
  const char* KernelName() const { return lmflow_ctx_kernel_name(c_); }
  void Log(LmflowLogLevel level, const char* msg) const { lmflow_ctx_log(c_, level, msg); }
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
  LmflowCloseReason CloseReason() const { return lmflow_ctx_close_reason(c_); }

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
  LmflowStatus RequireOption(const char* key, int64_t* o) const {
    return lmflow_ctx_require_option_i64(c_, key, o);
  }
  LmflowStatus RequireOption(const char* key, double* o) const {
    return lmflow_ctx_require_option_f64(c_, key, o);
  }
  LmflowStatus RequireOption(const char* key, bool* o) const {
    return lmflow_ctx_require_option_bool(c_, key, o);
  }
  LmflowStatus RequireOption(const char* key, const char** o) const {
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
  LmflowContext* c_;
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
  static_assert(std::is_base_of<Kernel, T>::value, "算子必须继承 lmflow::Kernel");

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

  static void get_contract(void*, LmflowContract* c) {
    if constexpr (internal::HasGetContract<T>::value) {
      try {
        Contract ct(c);
        T::GetContract(ct);
      } catch (...) {
      }
    }
    (void)c;
  }

  static LmflowStatus open(void* self, LmflowContext* c) {
    if (!self) return LMFLOW_ERR_KERNEL;  // create 失败(构造抛异常)
    try {
      Context cc(c);
      return static_cast<T*>(self)->Open(cc).code();
    } catch (...) {
      return LMFLOW_ERR_KERNEL;
    }
  }
  static LmflowStatus process(void* self, LmflowContext* c) {
    if (!self) return LMFLOW_ERR_KERNEL;
    try {
      Context cc(c);
      return static_cast<T*>(self)->Process(cc).code();
    } catch (...) {
      return LMFLOW_ERR_KERNEL;
    }
  }
  static LmflowStatus close(void* self, LmflowContext* c) {
    if (!self) return LMFLOW_ERR_KERNEL;
    try {
      Context cc(c);
      return static_cast<T*>(self)->Close(cc).code();
    } catch (...) {
      return LMFLOW_ERR_KERNEL;
    }
  }

  static const LmflowKernelVTable* vtable() {
    static const LmflowKernelVTable vt = {
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
 * 注意:静态初始化在静态库中可能被链接器裁剪。若发现算子「未注册」,
 * 改用显式聚合注册(见设计文档 §9)或链接时加 --whole-archive。
 */
#define LMFLOW_REGISTER_KERNEL_AS(T, name_str)                                             \
  namespace {                                                                            \
  struct LmflowReg_##T {                                                                   \
    LmflowReg_##T() { lmflow_register_kernel(name_str, lmflow::KernelAdapter<T>::vtable(), nullptr); } \
  };                                                                                     \
  static LmflowReg_##T g_flow_reg_##T;                                                     \
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
  }

#endif  // LMFLOW_HPP_
