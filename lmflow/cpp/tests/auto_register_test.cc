#include <cassert>
#include <cstring>

#include "lmflow/flow.h"

int main() {
  bool found = false;
  for (size_t i = 0; i < lmflow_registered_kernel_count(); ++i) {
    if (std::strcmp(lmflow_registered_kernel_name(i), "PassThroughKernel") == 0) {
      found = true;
      break;
    }
  }
  assert(found && "bundled C++ kernels must register when lmflow::lmflow is linked");

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
