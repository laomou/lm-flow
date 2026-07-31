/*
 * hello_world_host.cc —— 外部 C++ 宿主示例(只依赖 flow.h 这一层 C ABI)。
 *
 * 两级直通管线:
 *   input1 → node1(PassThrough) → input2 → node2(PassThrough) → output2
 *
 * 本文件不属于 cargo 构建(仓库内示例宿主是 Rust)。外部用户按自己的构建系统:
 *   g++ -std=c++17 -I<flow-core/include> hello_world_host.cc -lflow_core -lpthread
 */
#include <cstdio>
#include <cstdlib>

#include "flow.h"

/* 两个节点都未指定 executor → 都跑在主线程,任务在 poller_next / wait_done 期间被抽取。 */
static const char* kConfig = R"(
nodes:
  - name: "node1"
    kernel: "PassThroughKernel"
    input_ports: ["input1"]
    output_ports: ["input2"]
  - name: "node2"
    kernel: "PassThroughKernel"
    input_ports: ["input2"]
    output_ports: ["output2"]
input_ports: ["input1"]
output_ports: ["output2"]
)";

/* 把 int 打包成 FlowPacket:owner=NULL 表示「宿主新建」,提交后引擎接管并在引用归零时
 * 调用 drop_fn。type_id 用一个约定值(跨语言读值时两侧须一致,见 flow.h 说明);
 * 本例算子只做直通、不读值,故取 0(= 不声明类型)亦可。 */
static FlowPacket MakeInt(int value, int64_t ts) {
  FlowPacket p;
  p.payload = new int(value);
  p.type_id = 0;
  p.timestamp = ts;
  p.owner = nullptr;
  p.drop_fn = [](void* q) { delete static_cast<int*>(q); };
  return p;
}

#define CHECK(expr)                                                        \
  do {                                                                     \
    FlowStatus st_ = (expr);                                               \
    if (st_ != FLOW_OK) {                                                  \
      fprintf(stderr, "%s failed: %d (%s)\n", #expr, st_, flow_last_error()); \
      return 1;                                                            \
    }                                                                      \
  } while (0)

int main() {
  /* 动态链接时务必校验:header 与 .so 版本不一致会导致结构体布局错乱 */
  if (flow_abi_version() != FLOW_ABI_VERSION) {
    fprintf(stderr, "ABI 不匹配: lib=%u header=%u\n", flow_abi_version(), FLOW_ABI_VERSION);
    return 1;
  }

  /* 必须先注册内置算子,否则 init 会报「算子未注册」 */
  flow_register_builtin_kernels();

  FlowGraph* graph = flow_graph_new();
  if (!graph) {
    fprintf(stderr, "flow_graph_new: %s\n", flow_last_error());
    return 1;
  }

  CHECK(flow_graph_init_from_yaml(graph, kConfig));

  FlowPoller* poller = flow_graph_add_poller(graph, "output2");
  if (!poller) {
    fprintf(stderr, "add_poller: %s\n", flow_last_error());
    return 1;
  }

  CHECK(flow_graph_start(graph));

  /* 句柄式输入:热路径免去每包按名字查表(生命周期随 graph,无需释放) */
  FlowInput* input = flow_graph_input(graph, "input1");
  if (!input) {
    fprintf(stderr, "graph_input: %s\n", flow_last_error());
    return 1;
  }

  for (int i = 0; i < 10; ++i) {
    CHECK(flow_input_send(input, MakeInt(i, i)));

    FlowPacket out;
    if (!flow_poller_next(poller, &out)) break; /* 图已结束 */
    printf("out: %d @ ts=%lld\n", *static_cast<const int*>(out.payload),
           static_cast<long long>(out.timestamp));
    flow_packet_drop(&out); /* 语义 3:poller 移交所有权,宿主必须释放 */
  }

  flow_graph_close_all_inputs(graph);
  CHECK(flow_graph_wait_done(graph));

  flow_poller_free(poller);
  flow_graph_free(graph);
  return 0;
}
