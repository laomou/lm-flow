#include "lmflow/flow.hpp"

#include <cstdint>

int main() {
  const char* yaml = R"(
nodes:
  - { name: pass, kernel: PassThroughKernel, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
)";
  lmflow::Graph graph = lmflow::Graph::from_yaml(yaml);
  lmflow::Poller poller = graph.add_poller("out");
  lmflow::Input input = graph.input("in");
  if (!graph.start().ok()) return 2;
  if (!input.send(lmflow::Packet::FromI64(7).At(1)).ok()) return 3;
  auto packet = poller.next_timeout(1000);
  int64_t value = 0;
  if (!packet || !packet->AsI64(&value) || value != 7) return 4;
  input.close();
  return graph.wait_done().ok() ? 0 : 5;
}
