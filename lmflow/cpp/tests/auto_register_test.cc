#include <cstdio>

#include "lmflow/flow.h"

int main() {
  const char* type_name = "lmflow.test.DynamicBoundary";
  const uint64_t type_id = lmflow_type_id(type_name);
  if (type_id == 0) {
    std::fprintf(stderr, "lmflow_type_id failed: %s\n", lmflow_last_error());
    return 1;
  }
  if (lmflow_register_type_descriptor(type_id, type_name, 16, 8) != LMFLOW_OK ||
      lmflow_register_type_descriptor(type_id, type_name, 16, 8) != LMFLOW_OK) {
    std::fprintf(stderr, "type descriptor registration failed: %s\n", lmflow_last_error());
    return 2;
  }
  if (lmflow_type_size(type_id) != 16 || lmflow_type_align(type_id) != 8) {
    std::fprintf(stderr, "registered type descriptor is not queryable\n");
    return 3;
  }
  if (lmflow_register_type_descriptor(type_id, type_name, 24, 8) == LMFLOW_OK) {
    std::fprintf(stderr, "conflicting type layout was accepted\n");
    return 4;
  }
  if (lmflow_register_type_descriptor(type_id + 1, type_name, 16, 8) == LMFLOW_OK) {
    std::fprintf(stderr, "noncanonical type id was accepted\n");
    return 5;
  }

  LMFlowGraph* graph = lmflow_graph_new();
  if (graph == nullptr) {
    std::fprintf(stderr, "lmflow_graph_new failed: %s\n", lmflow_last_error());
    return 6;
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
    return 7;
  }
  lmflow_graph_free(graph);
  return 0;
}
