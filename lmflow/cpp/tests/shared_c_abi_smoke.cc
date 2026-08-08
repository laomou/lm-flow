#include "lmflow/flow.h"

int main() {
  LMFlowGraph* graph = lmflow_graph_new();
  if (!graph) return 1;
  const char* yaml = R"(
nodes:
  - { name: pass, kernel: PassThroughKernel, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
)";
  const LMFlowStatus status = lmflow_graph_init_from_yaml(graph, yaml);
  lmflow_graph_free(graph);
  return status == LMFLOW_OK ? 0 : 2;
}
