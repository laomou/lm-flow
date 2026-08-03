/*
 * custom_type_host.cc —— **自定义类型**端到端示例。
 *
 * 演示:让一个任意的 C++ 类型 `Detection` 在两个 C++ 算子之间的边上流动,契约用
 * `InputSet<T>` / `OutputSet<T>` 强制类型匹配。管线:
 *
 *     in(I64) → detect(DetectKernel) → mid(Detection) → report(ReportKernel) → out(I64)
 *
 * 关键点(见 docs/design.md §5 / flow.h 的类型系统说明):
 *   - 自定义类型只在**同语言(C++↔C++)**子图里流动:type_id 由 C++ typeid 算出,
 *     Python/其它语言侧产不出同一个标识(故跨语言请改用内建 BUFFER / STR+JSON)。
 *   - `LMFLOW_DECLARE_TYPE_NAME` 把 type_id 钉到一个稳定名(否则用修饰名,跨工具链不稳)。
 *   - `Packet::Make<T>` 把对象搬上边(引擎只搬指针 + drop_fn,不解读内容);
 *     下游 `Packet::Get<T>()` 类型安全取回;契约的 type_id 由引擎在收包时校验。
 *   - `LMFLOW_REGISTER_KERNEL(K)` 一行 self-register:在**你自己控制链接**的宿主里
 *     文件作用域写一句即可(main 之前静态注册),无需集中登记。引擎内置算子则用
 *     显式聚合(静态库会裁未引用的注册对象,见 ADR #14)。
 *
 * 构建见同目录 CMakeLists.txt。运行应打印 out: 0..5 并以退出码 0 结束。
 */
#include <cstdio>
#include <cstdlib>

#include "lmflow/flow.h"
#include "lmflow/flow.hpp"

// ---- 一个任意的 C++ 类型 + 稳定的跨工具链名字 ----
struct Detection {
  int64_t id;
  float score;
};
LMFLOW_DECLARE_TYPE_NAME(Detection, "example.Detection")

namespace {

// I64 → Detection:把标量包成自定义对象放上边。
class DetectKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.OutputSet<Detection>(0);  // 本口产出 Detection —— 契约按 type_id 声明
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t v = 0;
    if (!cc.Input(0).AsI64(&v)) return lmflow::Status::Ok();
    cc.Emit(0, lmflow::Packet::Make<Detection>(Detection{v, static_cast<float>(v) * 0.5f}));
    return lmflow::Status::Ok();
  }
};
LMFLOW_REGISTER_KERNEL(DetectKernel)  // 一行 self-register(名字取类名)—— main 之前静态注册

// Detection → I64:类型安全取回对象,读出字段。
class ReportKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSet<Detection>(0);  // 只接 Detection;收到别的 type_id 引擎会报错
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }
  lmflow::Status Process(lmflow::Context& cc) override {
    const Detection* d = cc.Input(0).TryGet<Detection>();
    if (!d) return lmflow::Status::Ok();
    cc.Emit(0, lmflow::Packet::FromI64(d->id));
    return lmflow::Status::Ok();
  }
};
LMFLOW_REGISTER_KERNEL(ReportKernel)

}  // namespace

static const char* kConfig = R"(
nodes:
  - { name: detect, kernel: DetectKernel, input_ports: ["in"],  output_ports: ["mid"] }
  - { name: report, kernel: ReportKernel, input_ports: ["mid"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
)";

#define CHECK(expr)                                                              \
  do {                                                                           \
    LMFlowStatus st_ = (expr);                                                   \
    if (st_ != LMFLOW_OK) {                                                      \
      fprintf(stderr, "%s failed: %d (%s)\n", #expr, st_, lmflow_last_error());  \
      return 1;                                                                  \
    }                                                                            \
  } while (0)

int main() {
  if (lmflow_abi_version() != LMFLOW_ABI_VERSION) {
    fprintf(stderr, "ABI mismatch: lib=%u header=%u\n", lmflow_abi_version(), LMFLOW_ABI_VERSION);
    return 1;
  }

  // 两个算子已由文件作用域的 LMFLOW_REGISTER_KERNEL 宏在 main 之前自注册(见上)——
  // 本例是自己控制链接的可执行,self-register 安全;引擎内置算子仍走显式聚合(ADR #14)。

  LMFlowGraph* graph = lmflow_graph_new();
  if (!graph) {
    fprintf(stderr, "lmflow_graph_new: %s\n", lmflow_last_error());
    return 1;
  }
  CHECK(lmflow_graph_init_from_yaml(graph, kConfig));

  LMFlowPoller* poller = lmflow_graph_add_poller(graph, "out");
  if (!poller) {
    fprintf(stderr, "add_poller: %s\n", lmflow_last_error());
    return 1;
  }
  CHECK(lmflow_graph_start(graph));

  LMFlowInput* input = lmflow_graph_input(graph, "in");
  if (!input) {
    fprintf(stderr, "graph_input: %s\n", lmflow_last_error());
    return 1;
  }

  int rc = 0;
  for (int64_t i = 0; i < 6; ++i) {
    // 送一个 I64;detect 把它包成 Detection,report 再取回 id —— 值应原样穿过自定义类型的边。
    CHECK(lmflow_input_send(input, lmflow::Packet::FromI64(i).At(i).release()));

    LMFlowPacket out;
    if (!lmflow_poller_next(poller, &out)) break;
    int64_t got = 0;
    lmflow_packet_as_i64(&out, &got);
    printf("out: %lld @ ts=%lld\n", (long long)got, (long long)out.timestamp);
    if (got != i) {
      fprintf(stderr, "roundtrip mismatch: expected %lld, got %lld\n", (long long)i, (long long)got);
      rc = 1;
    }
    lmflow_packet_drop(&out);
  }

  lmflow_graph_close_all_inputs(graph);
  CHECK(lmflow_graph_wait_done(graph));

  lmflow_input_free(input);
  lmflow_poller_free(poller);
  lmflow_graph_free(graph);
  if (rc == 0) printf("custom type Detection round-tripped through the graph OK\n");
  return rc;
}
