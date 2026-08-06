#include <cassert>

#include "lmflow/flow.h"

int main() {
  LMFlowGraph* graph = lmflow_graph_new();
  assert(graph != nullptr);
  const char* yaml = R"(
nodes:
  - { name: pass, kernel: PassThroughKernel, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
)";
  assert(lmflow_graph_init_from_yaml(graph, yaml) == LMFLOW_OK);
  lmflow_graph_free(graph);
  return 0;
}
