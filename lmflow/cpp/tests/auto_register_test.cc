#include <cstdio>

#include "lmflow/flow.h"

int main() {
  LMFlowGraph* graph = lmflow_graph_new();
  if (graph == nullptr) {
    std::fprintf(stderr, "lmflow_graph_new failed: %s\n", lmflow_last_error());
    return 1;
  }
  const char* yaml = R"(
nodes:
  - { name: pass, kernel: PassThroughKernel, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
)";
  const LMFlowStatus status = lmflow_graph_init_from_yaml(graph, yaml);
  if (status != LMFLOW_OK) {
    std::fprintf(stderr, "automatic kernel registration failed: %s\n", lmflow_last_error());
    lmflow_graph_free(graph);
    return 2;
  }
  lmflow_graph_free(graph);
  return 0;
}
