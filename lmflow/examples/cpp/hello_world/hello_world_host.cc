/*
 * hello_world_host.cc —— 外部 C++ 宿主示例。
 *
 * 两级直通管线:
 *   input1 → node1(PassThrough) → input2 → node2(PassThrough) → output2
 *
 * flow.hpp 是 flow.h C ABI 的 header-only RAII 包装，不直接依赖 Rust 内部实现。
 */
#include <cstdio>
#include <exception>

#include "lmflow/flow.hpp"

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

int main() {
  try {
    lmflow::Graph graph = lmflow::Graph::from_yaml(kConfig);
    lmflow::Poller poller = graph.add_poller("output2");
    lmflow::Input input = graph.input("input1");

    lmflow::Status status = graph.start();
    if (!status.ok()) throw std::runtime_error(lmflow_last_error());

    for (int64_t value = 0; value < 10; ++value) {
      status = input.send(lmflow::Packet::FromI64(value).At(value));
      if (!status.ok()) throw std::runtime_error(lmflow_last_error());

      auto output = poller.next();
      if (!output) break;

      int64_t result = 0;
      if (!output->AsI64(&result)) throw std::runtime_error("unexpected output type");
      std::printf("out: %lld @ ts=%lld\n", static_cast<long long>(result),
                  static_cast<long long>(output->Timestamp()));
    }

    input.close();
    status = graph.wait_done();
    if (!status.ok()) throw std::runtime_error(graph.last_error());
  } catch (const std::exception& error) {
    std::fprintf(stderr, "%s\n", error.what());
    return 1;
  }
  return 0;
}
