#include "lmflow/flow.h"

int main() {
  if (lmflow_abi_version() != LMFLOW_ABI_VERSION) return 1;
  const char* type_name = "lmflow.test.ExternalHost";
  const uint64_t type_id = lmflow_type_id(type_name);
  if (!type_id) return 2;
  if (lmflow_register_type_descriptor(type_id, type_name, 16, 8) != LMFLOW_OK) return 3;
  if (lmflow_register_type_descriptor(type_id, type_name, 24, 8) == LMFLOW_OK) return 4;
  LMFlowGraph* graph = lmflow_graph_new();
  if (!graph) return 5;
  const char* yaml = R"(
nodes:
  - { name: pass, kernel: PassThroughKernel, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
)";
  const LMFlowStatus status = lmflow_graph_init_from_yaml(graph, yaml);
  lmflow_graph_free(graph);
  return status == LMFLOW_OK ? 0 : 6;
}
