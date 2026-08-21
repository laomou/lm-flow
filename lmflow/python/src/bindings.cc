/*
 * bindings.cc —— lmflow 的 Python 扩展模块(pybind11)。
 *
 * 只依赖 include/flow.h 这一层 C ABI,和 C++ 算子走同一条路 ——
 * 引擎对「算子是什么语言写的」一无所知。
 *
 * 三件必须处理对的事:
 *
 *  1. **GIL**
 *     - 算子的 open/process/close 由引擎线程回调 → 进入 Python 前必须 acquire;
 *     - 宿主侧一切可能阻塞的接口(wait_done / poller.next / send / graph 析构)
 *       必须 release,否则引擎线程拿不到 GIL,直接死锁。
 *
 *  2. **异常不得穿越 FFI**
 *     Python 异常在蹦床里捕获并转成错误码 + lmflow_ctx_set_error,绝不让它逃出 extern "C"。
 *
 *  3. **解释器生命周期**
 *     图必须在解释器开始销毁之前停掉。Graph 提供上下文管理器与显式 close();
 *     析构里兜底,但不保证时机 —— 文档强调用 with。
 *
 * 数据类型:遵循「Python 算子只收发内建类型」(见 flow.h 的说明)。
 * 大块数值数据走 LMFlowBuffer,与 numpy 零拷贝互通。
 */
#include <pybind11/functional.h>
#include <pybind11/numpy.h>
#include <pybind11/pybind11.h>
#include <pybind11/stl.h>

#include <cmath>
#include <cstring>
#include <limits>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <vector>

#include "lmflow/flow.h"

namespace py = pybind11;

// ---------------------------------------------------------------- 工具

namespace {

[[noreturn]] void throw_timeout_error(const std::string& msg) {
  PyErr_SetString(PyExc_TimeoutError, msg.c_str());
  throw py::error_already_set();
}

/// LMFLOW_ERR_KERNEL 的载体,由模块初始化处注册成 `lmflow.KernelError`。
///
/// 派生自 `std::runtime_error` 是刻意的:Python 侧 KernelError 派生自 RuntimeError,
/// 于是既能 `except lmflow.KernelError` 精确接,也不破坏原先 `except RuntimeError` 的代码。
class KernelFailure : public std::runtime_error {
 public:
  using std::runtime_error::runtime_error;
};

/// 把 C ABI 的失败转成 Python 异常,并带上引擎给的可读原因。
void check(LMFlowStatus st, const char* what) {
  if (st == LMFLOW_OK) return;
  const char* detail = lmflow_last_error();
  std::string msg = std::string(what) + " failed (code=" + std::to_string(st) + ")";
  if (detail && *detail) msg += ": " + std::string(detail);
  switch (st) {
    case LMFLOW_ERR_TIMEOUT:
      throw_timeout_error(msg);
    case LMFLOW_ERR_KERNEL:
      throw KernelFailure(msg);
    case LMFLOW_ERR_INVALID_ARG:
    case LMFLOW_ERR_UNSUPPORTED:
      throw py::value_error(msg);
    case LMFLOW_ERR_NOT_FOUND:
      throw py::key_error(msg);
    default:
      throw std::runtime_error(msg);
  }
}

int dtype_from_numpy(const py::dtype& dt) {
  if (dt.is(py::dtype::of<uint8_t>())) return LMFLOW_DTYPE_U8;
  if (dt.is(py::dtype::of<int8_t>())) return LMFLOW_DTYPE_I8;
  if (dt.is(py::dtype::of<uint16_t>())) return LMFLOW_DTYPE_U16;
  if (dt.is(py::dtype::of<int16_t>())) return LMFLOW_DTYPE_I16;
  if (dt.is(py::dtype::of<int32_t>())) return LMFLOW_DTYPE_I32;
  if (dt.is(py::dtype::of<int64_t>())) return LMFLOW_DTYPE_I64;
  if (dt.is(py::dtype::of<float>())) return LMFLOW_DTYPE_F32;
  if (dt.is(py::dtype::of<double>())) return LMFLOW_DTYPE_F64;
  // float16:C++ 无标准 half 类型,按 numpy 的 kind('f')+itemsize(2) 辨认。
  // 放在 F32/F64 之后,不会误伤它们。fp16 是模型推理的主力类型,必须支持。
  if (dt.kind() == 'f' && dt.itemsize() == 2) return LMFLOW_DTYPE_F16;
  throw py::value_error(
      "unsupported numpy dtype; supported: "
      "uint8/int8/uint16/int16/int32/int64/float16/float32/float64");
}

py::dtype numpy_from_dtype(int dt) {
  switch (dt) {
    case LMFLOW_DTYPE_U8: return py::dtype::of<uint8_t>();
    case LMFLOW_DTYPE_I8: return py::dtype::of<int8_t>();
    case LMFLOW_DTYPE_U16: return py::dtype::of<uint16_t>();
    case LMFLOW_DTYPE_I16: return py::dtype::of<int16_t>();
    case LMFLOW_DTYPE_I32: return py::dtype::of<int32_t>();
    case LMFLOW_DTYPE_I64: return py::dtype::of<int64_t>();
    case LMFLOW_DTYPE_F16: return py::dtype("float16");
    case LMFLOW_DTYPE_F32: return py::dtype::of<float>();
    case LMFLOW_DTYPE_F64: return py::dtype::of<double>();
    default: throw py::value_error("unknown LMFLOW_DTYPE");
  }
}

/// 把 LMFlowBuffer 包成 numpy 视图(**零拷贝**)。
/// `owner` 让底层缓冲在数组存活期间不被释放。
py::array wrap_buffer(const LMFlowBuffer& b, const py::object& owner, bool writable) {
  std::vector<py::ssize_t> shape(b.shape, b.shape + b.ndim);
  std::vector<py::ssize_t> strides(b.strides, b.strides + b.ndim);
  auto dt = numpy_from_dtype(b.dtype);
  py::array arr(dt, shape, strides, b.data, owner);
  if (!writable) {
    // 用 numpy 自己的公开 API 清掉 writeable —— 比直接改 flags 位稳,
    // 也比依赖 pybind11 的 const 重载可靠(该重载并不清这个标志)。
    arr.attr("setflags")(py::arg("write") = false);
  }
  return arr;
}

struct NumpyBufferOwner {
  py::array array;
  bool restore_writable;
};

void release_numpy_buffer(void* user_data) {
  auto* owner = static_cast<NumpyBufferOwner*>(user_data);
  py::gil_scoped_acquire gil;
  if (owner->restore_writable) {
    try {
      owner->array.attr("setflags")(py::arg("write") = true);
    } catch (py::error_already_set& error) {
      error.discard_as_unraisable("lmflow: failed to restore adopted numpy array writability");
    }
  }
  delete owner;
}

}  // namespace

// 类名刻意不用 `Py*` 前缀:那是 CPython 的命名空间(例如 CPython 自己就有
// `PyContext`),占用它会撞名。绑定包装类型也不能放进公开 `lmflow` 命名空间,
// 否则与 flow.hpp 的 Packet / Context / Contract 在 LTO 下构成 ODR 冲突。
namespace lmflow_python {

// ---------------------------------------------------------------- Packet

/// Python 侧的数据包。`owned_` 决定析构是否归还引擎引用。
class Packet {
 public:
  Packet() { raw_ = LMFlowPacket{nullptr, 0, LMFLOW_TS_UNSET, nullptr, nullptr}; }
  explicit Packet(LMFlowPacket raw, bool owned) : raw_(raw), owned_(owned) {}

  Packet(const Packet&) = delete;
  Packet& operator=(const Packet&) = delete;

  ~Packet() { release(); }

  void release() {
    if (owned_) {
      lmflow_packet_drop(&raw_);
      owned_ = false;
    }
    raw_.payload = nullptr;
  }

  /// 交给引擎(emit/send):此后不再由本对象释放。
  LMFlowPacket surrender() {
    owned_ = false;
    return raw_;
  }

  const LMFlowPacket& raw() const { return raw_; }
  LMFlowPacket* raw_mut() { return &raw_; }

  int64_t timestamp() const { return raw_.timestamp; }
  void set_timestamp(int64_t ts) { raw_.timestamp = ts; }
  bool is_empty() const { return raw_.payload == nullptr; }
  uint64_t type_id() const { return raw_.type_id; }

  std::string type_name() const { return lmflow_type_name(raw_.type_id); }

  std::string repr() const {
    const char* s = lmflow_packet_debug_string(&raw_);
    return s ? std::string(s) : std::string("Packet{?}");
  }

  // ---- 内建类型读取 ----
  std::optional<int64_t> as_int() const {
    int64_t v = 0;
    return lmflow_packet_as_i64(&raw_, &v) ? std::optional<int64_t>(v) : std::nullopt;
  }
  std::optional<double> as_float() const {
    double v = 0;
    return lmflow_packet_as_f64(&raw_, &v) ? std::optional<double>(v) : std::nullopt;
  }
  std::optional<bool> as_bool() const {
    bool v = false;
    return lmflow_packet_as_bool(&raw_, &v) ? std::optional<bool>(v) : std::nullopt;
  }
  std::optional<std::string> as_str() const {
    const char* s = nullptr;
    return lmflow_packet_as_str(&raw_, &s) ? std::optional<std::string>(s) : std::nullopt;
  }
  std::optional<py::bytes> as_bytes() const {
    const void* d = nullptr;
    size_t n = 0;
    if (!lmflow_packet_as_bytes(&raw_, &d, &n)) return std::nullopt;
    return py::bytes(static_cast<const char*>(d), n);
  }

  void set_metadata(const std::string& key, const py::object& value) {
    LMFlowStatus status = LMFLOW_ERR_INVALID_ARG;
    if (py::isinstance<py::bool_>(value)) {
      status = lmflow_packet_set_metadata_bool(&raw_, key.c_str(), value.cast<bool>());
    } else if (py::isinstance<py::int_>(value)) {
      status = lmflow_packet_set_metadata_i64(&raw_, key.c_str(), value.cast<int64_t>());
    } else if (py::isinstance<py::float_>(value)) {
      status = lmflow_packet_set_metadata_f64(&raw_, key.c_str(), value.cast<double>());
    } else if (py::isinstance<py::str>(value)) {
      std::string text = value.cast<std::string>();
      status = lmflow_packet_set_metadata_str(&raw_, key.c_str(), text.c_str());
    } else {
      throw py::type_error("metadata value must be bool, int, float, or str");
    }
    if (status != LMFLOW_OK) throw std::runtime_error(lmflow_last_error());
  }

  py::object metadata(const std::string& key) const {
    bool boolean = false;
    if (lmflow_packet_metadata_bool(&raw_, key.c_str(), &boolean)) return py::bool_(boolean);
    int64_t integer = 0;
    if (lmflow_packet_metadata_i64(&raw_, key.c_str(), &integer)) return py::int_(integer);
    double floating = 0;
    if (lmflow_packet_metadata_f64(&raw_, key.c_str(), &floating)) return py::float_(floating);
    const char* text = nullptr;
    if (lmflow_packet_metadata_str(&raw_, key.c_str(), &text)) return py::str(text);
    return py::none();
  }

  bool has_metadata(const std::string& key) const {
    return lmflow_packet_has_metadata(&raw_, key.c_str());
  }

  bool remove_metadata(const std::string& key) {
    return lmflow_packet_remove_metadata(&raw_, key.c_str());
  }

  std::vector<std::string> metadata_keys() const {
    std::vector<std::string> keys;
    const size_t count = lmflow_packet_metadata_count(&raw_);
    keys.reserve(count);
    for (size_t index = 0; index < count; ++index) {
      const char* key = lmflow_packet_metadata_key_at(&raw_, index);
      if (key) keys.emplace_back(key);
    }
    return keys;
  }

  /// 只读 numpy 视图(零拷贝)。**仅在本包存活期间有效** ——
  /// 算子输入包是借用的,回调返回后不得再用。
  py::array as_numpy(const py::object& self) const {
    LMFlowBuffer b{};
    if (!lmflow_packet_as_buffer(&raw_, &b)) {
      throw py::value_error("this packet is not an LMFlowBuffer (construct it with new_buffer or from_numpy)");
    }
    return wrap_buffer(b, self, /*writable=*/false);
  }

  /// 可写 numpy 视图(写时复制)。独占则零拷贝;被共享才复制。
  /// 前置条件:本包为调用方所拥有 —— 典型来自 `Context.take_input`。
  py::array make_mutable(const py::object& self) {
    LMFlowBuffer b{};
    check(lmflow_packet_make_mutable_buffer(&raw_, &b), "make_mutable");
    return wrap_buffer(b, self, /*writable=*/true);
  }

  // ---- 内建类型构造 ----
  static Packet* from_int(int64_t v, int64_t ts) {
    return new Packet(lmflow_packet_from_i64(v, ts), true);
  }
  static Packet* from_float(double v, int64_t ts) {
    return new Packet(lmflow_packet_from_f64(v, ts), true);
  }
  static Packet* from_bool(bool v, int64_t ts) {
    return new Packet(lmflow_packet_from_bool(v, ts), true);
  }
  static Packet* from_str(const std::string& v, int64_t ts) {
    return new Packet(lmflow_packet_from_str(v.c_str(), ts), true);
  }
  static Packet* from_bytes(const py::bytes& v, int64_t ts) {
    py::buffer_info info(py::buffer(v).request());
    return new Packet(lmflow_packet_from_bytes(info.ptr, static_cast<size_t>(info.size), ts), true);
  }

  /// 零拷贝接管 numpy 数组。Packet 存活期间将传入数组标记为只读；
  /// 最后一个 Packet 引用释放时在 GIL 下恢复原 writeable 标志并归还 Python 引用。
  static Packet* from_numpy(const py::array& a, int64_t ts) {
    py::array arr = py::array::ensure(a);
    if (!arr) throw py::value_error("not a valid numpy array");
    if (arr.ndim() < 1 || arr.ndim() > LMFLOW_MAX_DIMS) {
      throw py::value_error("ndim must be in 1..=8");
    }
    LMFlowBuffer b{};
    b.data = const_cast<void*>(arr.data());
    b.ndim = static_cast<int32_t>(arr.ndim());
    b.dtype = dtype_from_numpy(arr.dtype());
    b.flags = LMFLOW_BUF_FLAG_READONLY;
    for (py::ssize_t i = 0; i < arr.ndim(); ++i) {
      b.shape[i] = arr.shape(i);
      b.strides[i] = arr.strides(i);
    }

    auto owner = std::make_unique<NumpyBufferOwner>(
        NumpyBufferOwner{arr, static_cast<bool>(arr.writeable())});
    if (owner->restore_writable) {
      arr.attr("setflags")(py::arg("write") = false);
    }
    LMFlowPacket raw =
        lmflow_packet_adopt_buffer(&b, ts, &release_numpy_buffer, owner.get());
    if (!raw.payload) {
      if (owner->restore_writable) {
        arr.attr("setflags")(py::arg("write") = true);
      }
      throw std::runtime_error(std::string("from_numpy failed: ") + lmflow_last_error());
    }
    owner.release();
    return new Packet(raw, true);
  }

 private:
  LMFlowPacket raw_{};
  bool owned_ = false;
};

/// 把 Python 值转成包:既接受 Packet,也接受裸的 int/float/bool/str/bytes/ndarray。
static LMFlowPacket to_flow_packet(const py::object& o, int64_t ts) {
  if (py::isinstance<Packet>(o)) {
    auto* p = o.cast<Packet*>();
    if (ts != LMFLOW_TS_UNSET) p->set_timestamp(ts);
    return p->surrender();
  }
  if (py::isinstance<py::array>(o)) {
    std::unique_ptr<Packet> p(Packet::from_numpy(o.cast<py::array>(), ts));
    return p->surrender();
  }
  if (py::isinstance<py::bool_>(o)) return lmflow_packet_from_bool(o.cast<bool>(), ts);
  if (py::isinstance<py::int_>(o)) return lmflow_packet_from_i64(o.cast<int64_t>(), ts);
  if (py::isinstance<py::float_>(o)) return lmflow_packet_from_f64(o.cast<double>(), ts);
  if (py::isinstance<py::bytes>(o)) {
    std::unique_ptr<Packet> p(Packet::from_bytes(o.cast<py::bytes>(), ts));
    return p->surrender();
  }
  if (py::isinstance<py::str>(o)) {
    return lmflow_packet_from_str(o.cast<std::string>().c_str(), ts);
  }
  throw py::type_error(
      "can only send a Packet or a built-in type (int/float/bool/str/bytes/ndarray). "
      "Python kernels cannot put arbitrary Python objects into the dataflow; "
      "for structured data use an N×K ndarray or a JSON string");
}

// ---------------------------------------------------------------- Contract

class Contract {
 public:
  explicit Contract(LMFlowContract* c) : c_(c) {}
  size_t num_inputs() const { return lmflow_contract_num_inputs(c_); }
  size_t num_outputs() const { return lmflow_contract_num_outputs(c_); }
  size_t input_id(const std::string& tag, size_t i) const {
    return lmflow_contract_input_id(c_, tag.c_str(), i);
  }
  size_t output_id(const std::string& tag, size_t i) const {
    return lmflow_contract_output_id(c_, tag.c_str(), i);
  }
  std::string input_name(size_t i) const { return lmflow_contract_input_name(c_, i); }
  std::string output_name(size_t i) const { return lmflow_contract_output_name(c_, i); }
  void input_set_any(size_t i) { lmflow_contract_input_set_any(c_, i); }
  void output_set_any(size_t i) { lmflow_contract_output_set_any(c_, i); }
  void input_set_type(size_t i, uint64_t t) { lmflow_contract_input_set_type(c_, i, t); }
  void output_set_type(size_t i, uint64_t t) { lmflow_contract_output_set_type(c_, i, t); }
  void require_side_packet(const std::string& n) {
    lmflow_contract_require_side_packet(c_, n.c_str());
  }

 private:
  LMFlowContract* c_;
};

// ---------------------------------------------------------------- Context

class Context {
 public:
  explicit Context(LMFlowContext* c) : c_(c) {}

  size_t num_inputs() const { return lmflow_ctx_num_inputs(c_); }
  size_t num_outputs() const { return lmflow_ctx_num_outputs(c_); }
  size_t input_id(const std::string& tag, size_t i) const {
    return lmflow_ctx_input_id(c_, tag.c_str(), i);
  }
  size_t output_id(const std::string& tag, size_t i) const {
    return lmflow_ctx_output_id(c_, tag.c_str(), i);
  }
  size_t input_index(const std::string& n) const { return lmflow_ctx_input_index(c_, n.c_str()); }
  std::string input_name(size_t i) const { return lmflow_ctx_input_name(c_, i); }
  std::string output_name(size_t i) const { return lmflow_ctx_output_name(c_, i); }

  std::string node_name() const { return lmflow_ctx_node_name(c_); }
  std::string kernel_name() const { return lmflow_ctx_kernel_name(c_); }
  int close_reason() const { return static_cast<int>(lmflow_ctx_close_reason(c_)); }

  bool input_is_empty(size_t i) const { return lmflow_ctx_input_is_empty(c_, i); }
  bool input_is_done(size_t i) const { return lmflow_ctx_input_is_done(c_, i); }
  int64_t input_timestamp() const { return lmflow_ctx_input_timestamp(c_); }

  /// 借用输入包 —— 回调返回后失效,不要留存。
  Packet* input(size_t i) const { return new Packet(lmflow_ctx_input(c_, i), /*owned=*/false); }

  /// 本次某输入口的包数(单包策略恒 0/1;`batch` 策略为该批大小)。
  size_t input_count(size_t i) const { return lmflow_ctx_input_count(c_, i); }
  /// 借用某输入口的第 k 个包(`batch` 策略下配合 input_count 遍历一批)。
  Packet* input_at(size_t i, size_t k) const {
    return new Packet(lmflow_ctx_input_at(c_, i, k), /*owned=*/false);
  }

  /// 取走输入包(所有权移交)。写时复制省拷贝的第一步。
  Packet* take_input(size_t i) { return new Packet(lmflow_ctx_take_input(c_, i), true); }

  void emit(size_t out, const py::object& value, std::optional<int64_t> ts) {
    lmflow_ctx_emit(c_, out, to_flow_packet(value, ts.value_or(LMFLOW_TS_UNSET)));
  }
  void forward(size_t in, size_t out) { lmflow_ctx_forward(c_, in, out); }
  void set_next_timestamp_bound(size_t out, int64_t b) {
    lmflow_ctx_set_next_ts_bound(c_, out, b);
  }

  /// 让**引擎**分配缓冲,返回 (packet, 可写 numpy 视图)。
  /// 推荐的产出路径:直接把结果写进引擎内存,不产生中间帧。
  py::tuple new_buffer(const std::vector<int64_t>& shape, const py::object& dtype);

  // ---- 参数 ----
  bool has_option(const std::string& k) const { return lmflow_ctx_has_option(c_, k.c_str()); }
  int64_t option_int(const std::string& k, int64_t d) const {
    return lmflow_ctx_option_i64(c_, k.c_str(), d);
  }
  double option_float(const std::string& k, double d) const {
    return lmflow_ctx_option_f64(c_, k.c_str(), d);
  }
  bool option_bool(const std::string& k, bool d) const {
    return lmflow_ctx_option_bool(c_, k.c_str(), d);
  }
  std::string option_str(const std::string& k, const std::string& d) const {
    const char* v = lmflow_ctx_option_str(c_, k.c_str(), d.c_str());
    return v ? v : d;
  }
  std::string options_json() const {
    const char* s = lmflow_ctx_options_json(c_);
    return s ? s : "{}";
  }
  /// 必需参数:缺失或类型不符直接抛异常 —— 让配置错误当场暴露,而不是静默用默认值。
  int64_t require_option_int(const std::string& k) const {
    int64_t v = 0;
    check(lmflow_ctx_require_option_i64(c_, k.c_str(), &v), "require_option_int");
    return v;
  }
  double require_option_float(const std::string& k) const {
    double v = 0;
    check(lmflow_ctx_require_option_f64(c_, k.c_str(), &v), "require_option_float");
    return v;
  }
  std::string require_option_str(const std::string& k) const {
    const char* v = nullptr;
    check(lmflow_ctx_require_option_str(c_, k.c_str(), &v), "require_option_str");
    return v ? v : "";
  }
  std::vector<int64_t> option_int_array(const std::string& k) const {
    size_t n = lmflow_ctx_option_count(c_, k.c_str());
    std::vector<int64_t> out(n);
    if (n) lmflow_ctx_option_i64_array(c_, k.c_str(), out.data(), n);
    return out;
  }
  std::vector<double> option_float_array(const std::string& k) const {
    size_t n = lmflow_ctx_option_count(c_, k.c_str());
    std::vector<double> out(n);
    if (n) lmflow_ctx_option_f64_array(c_, k.c_str(), out.data(), n);
    return out;
  }

  // ---- side packet / 日志 / 计数器 ----
  bool has_side_packet(const std::string& n) const {
    return lmflow_ctx_has_side_packet(c_, n.c_str());
  }
  Packet* side_packet(const std::string& n) const {
    return new Packet(lmflow_ctx_side_packet(c_, n.c_str()), false);
  }
  void log(int level, const std::string& msg) const {
    lmflow_ctx_log(c_, static_cast<LMFlowLogLevel>(level), msg.c_str());
  }
  void set_error(const std::string& msg) const { lmflow_ctx_set_error(c_, msg.c_str()); }
  void source_done() const { lmflow_ctx_source_done(c_); }
  void source_yield(double delay_seconds) const {
    constexpr double kMaxDelaySeconds =
        static_cast<double>(std::numeric_limits<uint64_t>::max()) / 1000.0;
    if (!std::isfinite(delay_seconds) || delay_seconds < 0.0 ||
        delay_seconds > kMaxDelaySeconds) {
      throw py::value_error("delay_seconds must be finite, non-negative, and representable");
    }
    lmflow_ctx_source_yield(c_, static_cast<uint64_t>(delay_seconds * 1000.0));
  }
  void counter_add(const std::string& n, int64_t d) const {
    lmflow_ctx_counter_add(c_, n.c_str(), d);
  }

 private:
  LMFlowContext* c_;
};

// ---------------------------------------------------------------- Python 算子蹦床

namespace {

struct PyKernelReg {
  py::object cls;
  std::string name;
};

/// 所有注册过的 Python 算子。**故意泄漏**:注册表要活到进程结束,
/// 而在解释器销毁后再释放 py::object 是未定义行为。
std::vector<PyKernelReg*>& py_registry() {
  static std::vector<PyKernelReg*> v;
  return v;
}

void* py_create(void* factory) {
  auto* reg = static_cast<PyKernelReg*>(factory);
  py::gil_scoped_acquire gil;
  try {
    return new py::object(reg->cls());
  } catch (py::error_already_set& e) {
    e.discard_as_unraisable("lmflow: Python kernel construction failed");
    return nullptr;
  }
}

void py_destroy(void* self) {
  if (!self) return;
  py::gil_scoped_acquire gil;
  delete static_cast<py::object*>(self);
}

/// 调 Python 算子的某个方法。异常一律转错误码,绝不穿越 extern "C"。
LMFlowStatus py_invoke(void* self, LMFlowContext* ctx, const char* method) {
  if (!self) return LMFLOW_ERR_KERNEL;
  // 引擎线程回调进 Python:必须先拿 GIL
  py::gil_scoped_acquire gil;
  try {
    auto* obj = static_cast<py::object*>(self);
    if (!py::hasattr(*obj, method)) return LMFLOW_OK;  // open/close 可选
    Context pc(ctx);
    obj->attr(method)(py::cast(pc, py::return_value_policy::move));
    return LMFLOW_OK;
  } catch (py::error_already_set& e) {
    lmflow_ctx_set_error(ctx, e.what());
    e.restore();
    PyErr_Clear();
    return LMFLOW_ERR_KERNEL;
  } catch (const std::exception& e) {
    lmflow_ctx_set_error(ctx, e.what());
    return LMFLOW_ERR_KERNEL;
  }
}

LMFlowStatus py_open(void* self, LMFlowContext* c) { return py_invoke(self, c, "open"); }
LMFlowStatus py_process(void* self, LMFlowContext* c) { return py_invoke(self, c, "process"); }
LMFlowStatus py_close(void* self, LMFlowContext* c) { return py_invoke(self, c, "close"); }

void py_get_contract(void* factory, LMFlowContract* out) {
  auto* reg = static_cast<PyKernelReg*>(factory);
  py::gil_scoped_acquire gil;
  try {
    if (!py::hasattr(reg->cls, "get_contract")) return;
    Contract pc(out);
    reg->cls.attr("get_contract")(py::cast(pc, py::return_value_policy::move));
  } catch (py::error_already_set& e) {
    const std::string message = e.what();
    lmflow_contract_set_error(out, message.c_str());
    e.restore();
    PyErr_Clear();
  } catch (const std::exception& e) {
    lmflow_contract_set_error(out, e.what());
  } catch (...) {
    lmflow_contract_set_error(out, "Python GetContract threw a non-standard exception");
  }
}

const LMFlowKernelVTable kPyVTable = {&py_create, &py_get_contract, &py_open,
                                    &py_process, &py_close,       &py_destroy};

void register_python_kernel(const std::string& name, const py::object& cls) {
  auto* reg = new PyKernelReg{cls, name};
  py_registry().push_back(reg);
  check(lmflow_register_kernel_with_language(name.c_str(), &kPyVTable, reg,
                                             LMFLOW_KERNEL_LANGUAGE_PYTHON),
        "register_kernel");
}

}  // namespace

// ---------------------------------------------------------------- Graph / Input / Poller

class Graph;

class Input {
 public:
  explicit Input(LMFlowInput* h) : h_(h) {}
  ~Input() { lmflow_input_free(h_); }
  Input(const Input&) = delete;
  Input& operator=(const Input&) = delete;

  /// 送包。可能因背压而阻塞 → **必须释放 GIL**,否则引擎线程拿不到 GIL 而死锁。
  void send(const py::object& value, std::optional<int64_t> ts) {
    LMFlowPacket p = to_flow_packet(value, ts.value_or(LMFLOW_TS_UNSET));
    LMFlowStatus st;
    {
      py::gil_scoped_release unlock;
      st = lmflow_input_send(h_, p);
    }
    check(st, "send");
  }
  bool try_send(const py::object& value, std::optional<int64_t> ts) {
    LMFlowPacket p = to_flow_packet(value, ts.value_or(LMFLOW_TS_UNSET));
    LMFlowStatus st;
    {
      py::gil_scoped_release unlock;
      st = lmflow_input_try_send(h_, p);
    }
    if (st == LMFLOW_ERR_WOULD_BLOCK) return false;
    check(st, "try_send");
    return true;
  }
  void close() {
    py::gil_scoped_release unlock;
    lmflow_input_close(h_);
  }
 private:
  LMFlowInput* h_;
};

class Poller {
 public:
  explicit Poller(LMFlowPoller* h) : h_(h) {}
  ~Poller() { lmflow_poller_free(h_); }
  Poller(const Poller&) = delete;
  Poller& operator=(const Poller&) = delete;

  /// 取下一个包。`timeout` 为 None 表示不限时。图结束返回 None,超时抛 TimeoutError。
  py::object next(std::optional<double> timeout) {
    LMFlowPacket out{};
    if (timeout.has_value()) {
      auto ms = static_cast<int64_t>(*timeout * 1000.0);
      LMFlowStatus st;
      {
        py::gil_scoped_release unlock;
        st = lmflow_poller_next_timeout(h_, &out, ms);
      }
      if (st == LMFLOW_ERR_TIMEOUT) {
        throw py::error_already_set();  // 由下方 translator 转 TimeoutError
      }
      if (st == LMFLOW_ERR_CLOSED) return py::none();
      check(st, "poller.next");
    } else {
      LMFlowStatus st;
      {
        py::gil_scoped_release unlock;
        st = lmflow_poller_next_status(h_, &out);
      }
      if (st == LMFLOW_ERR_CLOSED) return py::none();
      check(st, "poller.next");
    }
    return py::cast(new Packet(out, true), py::return_value_policy::take_ownership);
  }

  py::object try_next() {
    LMFlowPacket out{};
    LMFlowStatus st;
    {
      py::gil_scoped_release unlock;
      st = lmflow_poller_try_next_status(h_, &out);
    }
    if (st == LMFLOW_ERR_WOULD_BLOCK || st == LMFLOW_ERR_CLOSED) return py::none();
    check(st, "poller.try_next");
    return py::cast(new Packet(out, true), py::return_value_policy::take_ownership);
  }

 private:
  LMFlowPoller* h_;
};

class KernelRunner {
 public:
  KernelRunner(const std::string& kernel, std::size_t input_ports,
               std::size_t output_ports, const py::object& options,
               const py::dict& side_packets) {
    h_ = lmflow_kernel_runner_new(kernel.c_str(), input_ports, output_ports);
    if (!h_) throw std::runtime_error(std::string("KernelRunner failed: ") + lmflow_last_error());
    try {
      if (!options.is_none()) {
        const std::string json =
            py::module_::import("json").attr("dumps")(options).cast<std::string>();
        check(lmflow_kernel_runner_set_options_json(h_, json.c_str()),
              "KernelRunner.set_options");
      }
      for (auto item : side_packets) {
        const std::string name = py::cast<std::string>(item.first);
        check(lmflow_kernel_runner_set_side_packet(
                  h_, name.c_str(), to_flow_packet(item.second, LMFLOW_TS_UNSET)),
              "KernelRunner.set_side_packet");
      }
    } catch (...) {
      lmflow_kernel_runner_free(h_);
      h_ = nullptr;
      throw;
    }
  }
  ~KernelRunner() {
    if (h_) lmflow_kernel_runner_free(h_);
  }
  KernelRunner(const KernelRunner&) = delete;
  KernelRunner& operator=(const KernelRunner&) = delete;

  KernelRunner& enter() { return *this; }
  void exit(const py::args&) { close(); }
  void start() { check(lmflow_kernel_runner_start(h_), "KernelRunner.start"); }
  void add_input(std::size_t port, const py::object& value, std::optional<int64_t> ts) {
    check(lmflow_kernel_runner_add_input(
              h_, port, to_flow_packet(value, ts.value_or(LMFLOW_TS_UNSET))),
          "KernelRunner.add_input");
  }
  void process(int64_t timestamp) {
    check(lmflow_kernel_runner_process(h_, timestamp), "KernelRunner.process");
  }
  py::object try_next(std::size_t port) {
    LMFlowPacket out{};
    const LMFlowStatus status = lmflow_kernel_runner_try_next(h_, port, &out);
    if (status == LMFLOW_ERR_WOULD_BLOCK) return py::none();
    check(status, "KernelRunner.try_next");
    return py::cast(new Packet(out, true), py::return_value_policy::take_ownership);
  }
  void close() { check(lmflow_kernel_runner_close(h_), "KernelRunner.close"); }

 private:
  LMFlowKernelRunner* h_ = nullptr;
};

/// 图。**必须在解释器销毁前关闭** —— 用 with 语句,或显式 close()。
class Graph {
 public:
  Graph() {
    if (lmflow_abi_version() != LMFLOW_ABI_VERSION) {
      throw std::runtime_error("lmflow: ABI version mismatch; extension module and engine library versions differ");
    }
    g_ = lmflow_graph_new();
    if (!g_) throw std::runtime_error(std::string("lmflow_graph_new failed: ") + lmflow_last_error());
  }
  ~Graph() { close(); }

  Graph(const Graph&) = delete;
  Graph& operator=(const Graph&) = delete;

  void init_from_yaml(const std::string& yaml) {
    check(lmflow_graph_init_from_yaml(g_, yaml.c_str()), "init_from_yaml");
  }
  void init_from_yaml_file(const std::string& path) {
    check(lmflow_graph_init_from_yaml_file(g_, path.c_str()), "init_from_yaml_file");
  }

  void set_side_packet(const std::string& name, const py::object& value) {
    check(lmflow_graph_set_side_packet(g_, name.c_str(), to_flow_packet(value, LMFLOW_TS_UNSET)),
          "set_side_packet");
  }

  Poller* add_poller(const std::string& port, bool observe_timestamp_bounds) {
    LMFlowPoller* p =
        lmflow_graph_add_poller_ex(g_, port.c_str(), observe_timestamp_bounds);
    if (!p) throw py::key_error(std::string("add_poller failed: ") + lmflow_last_error());
    return new Poller(p);
  }

  void observe(const std::string& port, const py::function& fn,
               bool observe_timestamp_bounds) {
    // 回调对象需活到图销毁
    observers_.push_back(fn);
    auto* slot = &observers_.back();
    check(lmflow_graph_observe_ex(g_, port.c_str(), observe_timestamp_bounds,
                                 &observer_trampoline, slot),
          "observe");
  }

  void start() {
    LMFlowStatus status;
    {
      py::gil_scoped_release unlock;
      status = lmflow_graph_start(g_);
    }
    check(status, "start");
  }
  void reset() {
    LMFlowStatus status;
    {
      py::gil_scoped_release unlock;
      status = lmflow_graph_reset(g_);
    }
    check(status, "reset");
  }

  Input* input(const std::string& port) {
    LMFlowInput* h = lmflow_graph_input(g_, port.c_str());
    if (!h) throw py::key_error(std::string("graph.input failed: ") + lmflow_last_error());
    return new Input(h);
  }

  void close_input(const std::string& port) {
    LMFlowStatus status;
    {
      py::gil_scoped_release unlock;
      status = lmflow_graph_close_input(g_, port.c_str());
    }
    check(status, "close_input");
  }
  void close_all_inputs() {
    py::gil_scoped_release unlock;
    lmflow_graph_close_all_inputs(g_);
  }
  void cancel() {
    py::gil_scoped_release unlock;
    lmflow_graph_cancel(g_);
  }
  void pause() {
    py::gil_scoped_release unlock;
    lmflow_graph_pause(g_);
  }
  void resume() {
    py::gil_scoped_release unlock;
    lmflow_graph_resume(g_);
  }

  /// 等待图跑完。阻塞 → 释放 GIL。
  void wait_done(std::optional<double> timeout) {
    LMFlowStatus st;
    {
      py::gil_scoped_release unlock;
      st = timeout.has_value()
               ? lmflow_graph_wait_done_timeout(g_, static_cast<int64_t>(*timeout * 1000.0))
               : lmflow_graph_wait_done(g_);
    }
    check(st, "wait_done");
  }

  void wait_until_idle(std::optional<double> timeout) {
    LMFlowStatus st;
    {
      py::gil_scoped_release unlock;
      st = timeout.has_value()
               ? lmflow_graph_wait_until_idle_timeout(g_, static_cast<int64_t>(*timeout * 1000.0))
               : lmflow_graph_wait_until_idle(g_);
    }
    check(st, "wait_until_idle");
  }

  bool pump_step() {
    py::gil_scoped_release unlock;
    return lmflow_graph_pump_step(g_);
  }
  std::size_t pump_steps(std::size_t max_steps) {
    py::gil_scoped_release unlock;
    return lmflow_graph_pump_steps(g_, max_steps);
  }

  void set_wakeup_callback(const py::object& callback) {
    if (callback.is_none()) {
      LMFlowStatus status;
      {
        py::gil_scoped_release unlock;
        status = lmflow_graph_set_wakeup_callback(g_, nullptr, nullptr);
      }
      check(status, "set_wakeup_callback");
      wakeup_callback_.reset();
      return;
    }
    wakeup_callback_ = py::reinterpret_borrow<py::function>(callback);
    LMFlowStatus status;
    {
      py::gil_scoped_release unlock;
      status = lmflow_graph_set_wakeup_callback(g_, &wakeup_trampoline, this);
    }
    check(status, "set_wakeup_callback");
  }

  int state() const { return static_cast<int>(lmflow_graph_state(g_)); }
  std::string to_dot_view(int view) const {
    const char* s = lmflow_graph_to_dot_view(g_, static_cast<LMFlowDotView>(view));
    if (!s || s[0] == '\0') check(LMFLOW_ERR_INVALID_ARG, "to_dot_view");
    return s;
  }
  std::string last_error() const {
    const char* s = lmflow_graph_last_error(g_);
    return s ? s : "";
  }
  uint64_t dropped_count(const std::string& p) const {
    return lmflow_graph_dropped_count(g_, p.c_str());
  }
  int64_t counter_value(const std::string& n) const {
    return lmflow_graph_counter_value(g_, n.c_str());
  }
  /// 宿主侧分配引擎缓冲:返回 (packet, 可写 numpy 视图)。
  py::tuple new_buffer(const std::vector<int64_t>& shape, const py::object& dtype);

  /// 显式关闭。幂等。**释放 GIL**:内部会 cancel + 等待工作线程收尾,
  /// 而工作线程可能正在回调 Python(需要 GIL)。
  void close() {
    if (!g_) return;
    LMFlowGraph* g = g_;
    g_ = nullptr;
    {
      py::gil_scoped_release unlock;
      lmflow_graph_set_wakeup_callback(g, nullptr, nullptr);
      lmflow_graph_free(g);
    }
    wakeup_callback_.reset();
    observers_.clear();
  }

 private:
  static void observer_trampoline(void* user, LMFlowPacket pkt) {
    auto* fn = static_cast<py::function*>(user);
    py::gil_scoped_acquire gil;
    try {
      // 借用形态:回调返回后失效
      auto* p = new Packet(pkt, /*owned=*/false);
      (*fn)(py::cast(p, py::return_value_policy::take_ownership));
    } catch (py::error_already_set& e) {
      e.discard_as_unraisable("lmflow: observer callback raised an exception");
    }
  }

  static void wakeup_trampoline(void* user) {
    auto* self = static_cast<Graph*>(user);
    py::gil_scoped_acquire gil;
    if (!self->wakeup_callback_) return;
    try {
      (*self->wakeup_callback_)();
    } catch (py::error_already_set& error) {
      error.discard_as_unraisable("lmflow: wakeup callback raised an exception");
    }
  }

  LMFlowGraph* g_ = nullptr;
  std::list<py::function> observers_;
  std::optional<py::function> wakeup_callback_;
};

py::tuple Context::new_buffer(const std::vector<int64_t>& shape, const py::object& dtype) {
  LMFlowBuffer b{};
  int dt = dtype_from_numpy(py::dtype::from_args(dtype));
  LMFlowPacket raw = lmflow_packet_new_buffer(static_cast<int32_t>(shape.size()), shape.data(), dt,
                                          LMFLOW_TS_UNSET, &b);
  if (!raw.payload) throw std::runtime_error(std::string("new_buffer failed: ") + lmflow_last_error());
  auto* p = new Packet(raw, true);
  py::object owner = py::cast(p, py::return_value_policy::take_ownership);
  return py::make_tuple(owner, wrap_buffer(b, owner, /*writable=*/true));
}

py::tuple Graph::new_buffer(const std::vector<int64_t>& shape, const py::object& dtype) {
  LMFlowBuffer b{};
  int dt = dtype_from_numpy(py::dtype::from_args(dtype));
  LMFlowPacket raw = lmflow_packet_new_buffer(static_cast<int32_t>(shape.size()), shape.data(), dt,
                                          LMFLOW_TS_UNSET, &b);
  if (!raw.payload) throw std::runtime_error(std::string("new_buffer failed: ") + lmflow_last_error());
  auto* p = new Packet(raw, true);
  py::object owner = py::cast(p, py::return_value_policy::take_ownership);
  return py::make_tuple(owner, wrap_buffer(b, owner, /*writable=*/true));
}

// ---------------------------------------------------------------- 模块

// 仅当以 --with-cv-test 构建时:CV 测试算子(链 OpenCV)被编进本扩展,
// 由 cpp/tests/cv_test_register.cc 提供这个 extern "C" 注册入口。生产构建里它不存在,
// 扩展保持零 OpenCV 依赖(ADR #14)。
#ifdef LMFLOW_WITH_CV_TEST
extern "C" void lmflow_register_cv_test_kernels(void);
#endif

namespace {
py::object g_log_cb;

void log_trampoline(void* /*user*/, LMFlowLogLevel level, const char* msg) {
  py::gil_scoped_acquire gil;
  if (!g_log_cb || g_log_cb.is_none()) return;
  try {
    g_log_cb(static_cast<int>(level), std::string(msg ? msg : ""));
  } catch (py::error_already_set& e) {
    e.discard_as_unraisable("lmflow: log callback raised an exception");
  }
}
}  // namespace

}  // namespace lmflow_python

using namespace lmflow_python;

PYBIND11_MODULE(_lmflow, m) {
  m.doc() = "Python bindings for the lmflow engine (pybind11)";

  // 算子失败(LMFLOW_ERR_KERNEL)有自己的异常类,好让宿主把「算子炸了」与 cancel / 状态错误
  // 区分开 —— 从前它们一律塌成裸 RuntimeError。基类取 RuntimeError,故旧代码不受影响。
  py::register_exception<KernelFailure>(m, "KernelError", PyExc_RuntimeError);

  m.attr("ABI_VERSION") = LMFLOW_ABI_VERSION;
  m.attr("TS_UNSET") = LMFLOW_TS_UNSET;
  m.attr("TS_PRE_STREAM") = LMFLOW_TS_PRE_STREAM;
  m.attr("TS_POST_STREAM") = LMFLOW_TS_POST_STREAM;
  m.attr("TS_DONE") = LMFLOW_TS_DONE;
  m.attr("INVALID_ID") = static_cast<uint64_t>(LMFLOW_INVALID_ID);

  m.attr("CLOSE_NORMAL") = static_cast<int>(LMFLOW_CLOSE_NORMAL);
  m.attr("CLOSE_ERROR") = static_cast<int>(LMFLOW_CLOSE_ERROR);
  m.attr("CLOSE_CANCELLED") = static_cast<int>(LMFLOW_CLOSE_CANCELLED);

  m.def("abi_version", &lmflow_abi_version);
#ifdef LMFLOW_WITH_CV_TEST
  m.def("register_cv_test_kernels", [] { lmflow_register_cv_test_kernels(); },
        "Test-only: register the CV kernel (CvInvertTest); present only when the extension is built with --with-cv-test");
#endif
  m.def("register_kernel", &register_python_kernel, py::arg("name"), py::arg("cls"),
        "Register a Python class as a kernel");
  m.def(
      "set_log_callback",
      [](const py::object& cb) {
        g_log_cb = cb;
        if (cb.is_none()) {
          lmflow_set_log_callback(nullptr, nullptr);
        } else {
          lmflow_set_log_callback(&log_trampoline, nullptr);
        }
      },
      py::arg("cb"), "Set the log callback fn(level, msg); pass None to go silent");
  m.def(
      "type_id",
      [](const std::string& stable_name) {
        const uint64_t id = lmflow_type_id(stable_name.c_str());
        if (id == 0) check(LMFLOW_ERR_INVALID_ARG, "type_id");
        return id;
      },
      py::arg("stable_name"), "Compute the canonical id for a stable custom type name.");
  m.def("type_name", [](uint64_t id) { return std::string(lmflow_type_name(id)); });

  py::class_<Packet>(
      m, "Packet",
      "Immutable timestamped data packet; passed by reference across kernels "
      "(clone only bumps the refcount, no data copy).")
      .def(py::init<>())
      .def_property("timestamp", &Packet::timestamp, &Packet::set_timestamp, "Packet timestamp (int).")
      .def_property_readonly("is_empty", &Packet::is_empty, "Whether this is an empty packet.")
      .def_property_readonly("type_id", &Packet::type_id, "Payload type id.")
      .def_property_readonly("type_name", &Packet::type_name, "Human-readable payload type name.")
      .def("as_int", &Packet::as_int, "Read the int payload; None on type mismatch.")
      .def("set_metadata", &Packet::set_metadata, py::arg("key"), py::arg("value"))
      .def("metadata", &Packet::metadata, py::arg("key"))
      .def("has_metadata", &Packet::has_metadata, py::arg("key"))
      .def("remove_metadata", &Packet::remove_metadata, py::arg("key"))
      .def("metadata_keys", &Packet::metadata_keys)
      .def("as_float", &Packet::as_float, "Read the float payload; None on type mismatch.")
      .def("as_bool", &Packet::as_bool, "Read the bool payload; None on type mismatch.")
      .def("as_str", &Packet::as_str, "Read the str payload; None on type mismatch.")
      .def("as_bytes", &Packet::as_bytes, "Read the bytes payload; None on type mismatch.")
      .def(
          "as_numpy", [](const py::object& self) { return self.cast<Packet&>().as_numpy(self); },
          "Read-only numpy view (zero-copy); valid only while this packet is alive")
      .def(
          "make_mutable",
          [](const py::object& self) { return self.cast<Packet&>().make_mutable(self); },
          "Writable numpy view (copy-on-write: zero-copy when exclusively owned)")
      .def("__repr__", &Packet::repr)
      .def_static("from_int", &Packet::from_int, py::arg("value"), py::arg("ts") = LMFLOW_TS_UNSET,
                  "Make a packet from an int.")
      .def_static("from_float", &Packet::from_float, py::arg("value"),
                  py::arg("ts") = LMFLOW_TS_UNSET, "Make a packet from a float.")
      .def_static("from_bool", &Packet::from_bool, py::arg("value"),
                  py::arg("ts") = LMFLOW_TS_UNSET, "Make a packet from a bool.")
      .def_static("from_str", &Packet::from_str, py::arg("value"), py::arg("ts") = LMFLOW_TS_UNSET,
                  "Make a packet from a str.")
      .def_static("from_bytes", &Packet::from_bytes, py::arg("value"),
                  py::arg("ts") = LMFLOW_TS_UNSET, "Make a packet from bytes.")
      .def_static("from_numpy", &Packet::from_numpy, py::arg("array"),
                  py::arg("ts") = LMFLOW_TS_UNSET,
                  "Adopt a numpy array without copying; it stays read-only until the final packet "
                  "reference is released");

  py::class_<Contract>(
      m, "Contract",
      "Kernel port contract; declare port types and required side packets in get_contract(c).")
      .def_property_readonly("num_inputs", &Contract::num_inputs)
      .def_property_readonly("num_outputs", &Contract::num_outputs)
      .def("input_id", &Contract::input_id, py::arg("tag"), py::arg("index") = 0)
      .def("output_id", &Contract::output_id, py::arg("tag"), py::arg("index") = 0)
      .def("input_name", &Contract::input_name)
      .def("output_name", &Contract::output_name)
      .def("input_set_any", &Contract::input_set_any, "Declare an input port accepts any type.")
      .def("output_set_any", &Contract::output_set_any, "Declare an output port emits any type.")
      .def("input_set_type", &Contract::input_set_type, "Declare an input port's type constraint.")
      .def("output_set_type", &Contract::output_set_type, "Declare an output port's type.")
      .def("require_side_packet", &Contract::require_side_packet,
           "Declare a required side-packet name.");

  py::class_<Context>(
      m, "Context",
      "Per-invocation context for a kernel's open/process/close: read inputs and options, write outputs.")
      .def_property_readonly("num_inputs", &Context::num_inputs)
      .def_property_readonly("num_outputs", &Context::num_outputs)
      .def_property_readonly("node_name", &Context::node_name)
      .def_property_readonly("kernel_name", &Context::kernel_name)
      .def_property_readonly("close_reason", &Context::close_reason, "Why close fired (see CloseReason).")
      .def_property_readonly("input_timestamp", &Context::input_timestamp,
                             "The aligned timestamp of this invocation.")
      .def("input_id", &Context::input_id, py::arg("tag"), py::arg("index") = 0)
      .def("output_id", &Context::output_id, py::arg("tag"), py::arg("index") = 0)
      .def("input_index", &Context::input_index)
      .def("input_name", &Context::input_name)
      .def("output_name", &Context::output_name)
      .def("input_is_empty", &Context::input_is_empty, "Whether this input port has no packet this round.")
      .def("input_is_done", &Context::input_is_done, "Whether this input port is closed and drained.")
      .def("input", &Context::input, py::arg("index"), "Borrow the input packet at index (does not take it).")
      .def("input_count", &Context::input_count, py::arg("index"),
           "Number of packets at this input port this call (1 normally; the batch size under the `batch` policy).")
      .def("input_at", &Context::input_at, py::arg("index"), py::arg("k"),
           "Borrow the k-th packet at this input port (iterate a batch together with input_count).")
      .def("take_input", &Context::take_input, py::arg("index"),
           "Take the input packet at index (enables zero-copy in-place rewrite).")
      .def("emit", &Context::emit, py::arg("index"), py::arg("value"), py::arg("ts") = std::nullopt,
           "Emit a packet on output port index (defaults to the input timestamp).")
      .def("forward", &Context::forward, py::arg("in_index"), py::arg("out_index"),
           "Zero-copy forward an input packet to an output port.")
      .def("set_next_timestamp_bound", &Context::set_next_timestamp_bound)
      .def("new_buffer", &Context::new_buffer, py::arg("shape"), py::arg("dtype"),
           "Engine-side buffer allocation; returns (packet, writable numpy view) (zero-copy).")
      .def("has_option", &Context::has_option)
      .def("option_int", &Context::option_int, py::arg("key"), py::arg("default") = 0,
           "Read an int node option.")
      .def("option_float", &Context::option_float, py::arg("key"), py::arg("default") = 0.0,
           "Read a float node option.")
      .def("option_bool", &Context::option_bool, py::arg("key"), py::arg("default") = false,
           "Read a bool node option.")
      .def("option_str", &Context::option_str, py::arg("key"), py::arg("default") = "",
           "Read a str node option.")
      .def("option_int_array", &Context::option_int_array)
      .def("option_float_array", &Context::option_float_array)
      .def("options_json", &Context::options_json, "The whole options block as a raw JSON string.")
      .def("require_option_int", &Context::require_option_int)
      .def("require_option_float", &Context::require_option_float)
      .def("require_option_str", &Context::require_option_str)
      .def("has_side_packet", &Context::has_side_packet)
      .def("side_packet", &Context::side_packet, "Get a side packet (constant input).")
      .def("log", &Context::log, py::arg("level"), py::arg("msg"), "Write an engine log line.")
      .def("set_error", &Context::set_error, "Mark this invocation as failed, with a reason.")
      .def("source_done", &Context::source_done,
           "Source kernel (0 inputs): signal it has produced all its data -- the engine stops "
           "invoking it and closes its outputs, ending the graph.")
      .def("source_yield", &Context::source_yield, py::arg("delay_seconds") = 0.0,
           "Source kernel (0 inputs): release the worker and retry after the delay.")
      .def("counter_add", &Context::counter_add, py::arg("name"), py::arg("delta") = 1,
           "Add to a self-reported counter (for diagnostics).");

  py::class_<Input>(m, "Input",
                    "Graph input-port handle (avoids per-packet name lookup on the hot path).")
      .def("send", &Input::send, py::arg("value"), py::arg("ts") = std::nullopt,
           "Send a packet (blocking: waits at the watermark). ts defaults to auto-increment; "
           "releases the GIL while waiting.")
      .def("try_send", &Input::try_send, py::arg("value"), py::arg("ts") = std::nullopt,
           "Non-blocking send; raises at the watermark instead of blocking.")
      .def("close", &Input::close, "Close this input port.");

  py::class_<Poller>(m, "Poller",
                     "Pull-mode output handle; iterable directly (stops when the graph ends).")
      .def("next", &Poller::next, py::arg("timeout") = std::nullopt,
           "Get the next output packet; None when the graph ends, raises Timeout on timeout. "
           "Releases the GIL while waiting.")
      .def("try_next", &Poller::try_next,
           "Non-blocking get; returns None if empty or closed, raises KernelError if the graph failed.");

  py::class_<KernelRunner>(m, "KernelRunner",
                           "Single-kernel test harness; does not create a Graph.")
      .def(py::init<const std::string&, std::size_t, std::size_t, const py::object&,
                    const py::dict&>(),
           py::arg("kernel"), py::arg("input_ports") = 1,
           py::arg("output_ports") = 1, py::arg("options") = py::none(),
           py::arg("side_packets") = py::dict())
      .def("start", &KernelRunner::start)
      .def("__enter__", &KernelRunner::enter, py::return_value_policy::reference_internal)
      .def("__exit__", &KernelRunner::exit)
      .def("add_input", &KernelRunner::add_input, py::arg("port"), py::arg("value"),
           py::arg("ts") = std::nullopt)
      .def("process", &KernelRunner::process, py::arg("timestamp"))
      .def("try_next", &KernelRunner::try_next, py::arg("port") = 0)
      .def("close", &KernelRunner::close);

  py::class_<Graph>(m, "Graph")
      .def(py::init<>())
      .def("init_from_yaml", &Graph::init_from_yaml)
      .def("init_from_yaml_file", &Graph::init_from_yaml_file)
      .def("set_side_packet", &Graph::set_side_packet)
      .def("add_poller", &Graph::add_poller, py::arg("port"),
           py::arg("observe_timestamp_bounds") = false)
      .def("observe", &Graph::observe, py::arg("port"), py::arg("fn"),
           py::arg("observe_timestamp_bounds") = false)
      .def("start", &Graph::start)
      .def("reset", &Graph::reset)
      .def("input", &Graph::input)
      .def("close_input", &Graph::close_input)
      .def("close_all_inputs", &Graph::close_all_inputs)
      .def("cancel", &Graph::cancel)
      .def("pause", &Graph::pause)
      .def("resume", &Graph::resume)
      .def("wait_done", &Graph::wait_done, py::arg("timeout") = std::nullopt)
      .def("wait_until_idle", &Graph::wait_until_idle, py::arg("timeout") = std::nullopt)
      .def("pump_step", &Graph::pump_step)
      .def("pump_steps", &Graph::pump_steps, py::arg("max_steps"),
           "Run at most max_steps delegated tasks or close-progress steps.")
      .def("set_wakeup_callback", &Graph::set_wakeup_callback, py::arg("callback"),
           "Install a thread-safe event-loop wakeup callback; pass None to remove it.\n\n"
           "There is only ONE slot per graph: installing again replaces the previous callback, "
           "and the notification is graph-global (\"something progressed\"), not per-port. "
           "Graph.run_async() owns this slot today. Anything else that needs waking -- an async "
           "output iterator, an async send that waits for queue space -- must go through a shared "
           "dispatcher that owns the slot and broadcasts to its waiters; calling this a second "
           "time silently displaces run_async()'s callback and the graph quietly stops advancing."
           "\n\n"
           "Before building that dispatcher, check whether you need this slot at all: to consume "
           "output, use Graph.observe(), which already supports several subscribers per port and "
           "is a separate mechanism that does not touch this slot. The trade is its own "
           "constraints -- register before start(), cannot be removed afterwards, and the callback "
           "runs inline on the dispatching worker thread with a borrowed packet. Reserve this slot "
           "for what has no dedicated subscription API: waiting for the run to finish, or for an "
           "input queue to drain.")
      .def("new_buffer", &Graph::new_buffer, py::arg("shape"), py::arg("dtype"))
      .def_property_readonly("state", &Graph::state)
      .def("to_dot_view", &Graph::to_dot_view, py::arg("view"))
      .def("last_error", &Graph::last_error)
      .def("dropped_count", &Graph::dropped_count)
      .def("counter_value", &Graph::counter_value)
      .def("close", &Graph::close);
}
