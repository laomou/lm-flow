#include <cstdio>
#include <optional>

#include "lmflow/flow.hpp"

int main() {
  const char* yaml = R"(
nodes:
  - { name: pass, kernel: PassThroughKernel, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
)";

  try {
    lmflow::Graph graph = lmflow::Graph::from_yaml(yaml);
    lmflow::Poller poller = graph.add_poller("out");
    lmflow::Input input = graph.input("in");

    if (!graph.start().ok()) {
      std::fprintf(stderr, "start failed: %s\n", lmflow_last_error());
      return 1;
    }
    if (poller.next_timeout(0).has_value()) {
      std::fprintf(stderr, "unexpected packet before send\n");
      return 2;
    }
    if (!input.send(lmflow::Packet::FromI64(42).At(7)).ok()) {
      std::fprintf(stderr, "send failed: %s\n", lmflow_last_error());
      return 3;
    }

    std::optional<lmflow::Packet> output = poller.next();
    int64_t value = 0;
    if (!output || !output->AsI64(&value) || value != 42 || output->Timestamp() != 7) {
      std::fprintf(stderr, "unexpected output\n");
      return 4;
    }

    input.close();
    if (!graph.wait_done().ok()) {
      std::fprintf(stderr, "wait_done failed: %s\n", graph.last_error());
      return 5;
    }
    if (graph.state() != LMFLOW_STATE_TERMINATED) {
      std::fprintf(stderr, "unexpected terminal state\n");
      return 6;
    }
  } catch (const std::exception& error) {
    std::fprintf(stderr, "%s\n", error.what());
    return 7;
  }

  return 0;
}
