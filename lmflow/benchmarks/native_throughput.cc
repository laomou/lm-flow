#include "lmflow/flow.h"

#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <iomanip>
#include <iostream>
#include <stdexcept>
#include <string>

namespace {

using Clock = std::chrono::steady_clock;

void Check(LMFlowStatus status, const char* operation) {
  if (status != LMFLOW_OK) {
    throw std::runtime_error(std::string(operation) + ": " + lmflow_last_error());
  }
}

struct GraphRun {
  LMFlowGraph* graph = nullptr;
  LMFlowInput* input = nullptr;
  LMFlowPoller* poller = nullptr;

  explicit GraphRun(const char* kernel) {
    const std::string yaml =
        "max_queue_size: 1000000\n"
        "executors:\n"
        "  - { name: \"host\", type: \"DelegatingExecutor\" }\n"
        "nodes:\n"
        "  - { name: \"node\", kernel: \"" +
        std::string(kernel) +
        "\", executor: \"host\", input_ports: [\"in\"], output_ports: [\"out\"] }\n"
        "input_ports: [\"in\"]\n"
        "output_ports: [\"out\"]\n";

    graph = lmflow_graph_new();
    if (graph == nullptr) throw std::runtime_error(lmflow_last_error());
    Check(lmflow_graph_init_from_yaml(graph, yaml.c_str()), "init_from_yaml");
    poller = lmflow_graph_add_poller(graph, "out");
    if (poller == nullptr) throw std::runtime_error(lmflow_last_error());
    input = lmflow_graph_input(graph, "in");
    if (input == nullptr) throw std::runtime_error(lmflow_last_error());
    Check(lmflow_graph_start(graph), "start");
  }

  GraphRun(const GraphRun&) = delete;
  GraphRun& operator=(const GraphRun&) = delete;

  ~GraphRun() {
    lmflow_input_free(input);
    lmflow_poller_free(poller);
    lmflow_graph_free(graph);
  }

  void RoundTrip(LMFlowPacket packet) {
    Check(lmflow_input_send(input, packet), "send");
    LMFlowPacket output{};
    if (!lmflow_poller_next(poller, &output)) {
      throw std::runtime_error(std::string("poller.next: ") + lmflow_last_error());
    }
    lmflow_packet_drop(&output);
  }
};

struct OwnedPacket {
  LMFlowPacket packet{};

  explicit OwnedPacket(LMFlowPacket value) : packet(value) {}
  OwnedPacket(const OwnedPacket&) = delete;
  OwnedPacket& operator=(const OwnedPacket&) = delete;
  ~OwnedPacket() { lmflow_packet_drop(&packet); }
};

template <typename MakePacket>
void RunBenchmark(const char* name, std::size_t iterations, std::size_t payload_bytes,
                  MakePacket make_packet, const char* kernel) {
  GraphRun run(kernel);
  const std::size_t warmup = iterations / 10 + 1;
  int64_t timestamp = 0;

  for (std::size_t index = 0; index < warmup; ++index) {
    run.RoundTrip(make_packet(timestamp++));
  }

  const auto begin = Clock::now();
  for (std::size_t index = 0; index < iterations; ++index) {
    run.RoundTrip(make_packet(timestamp++));
  }
  const auto elapsed = Clock::now() - begin;
  const double seconds = std::chrono::duration<double>(elapsed).count();
  const double packets_per_second = static_cast<double>(iterations) / seconds;
  const double nanoseconds_per_packet = seconds * 1e9 / static_cast<double>(iterations);

  std::cout << std::left << std::setw(34) << name << std::right << std::fixed
            << std::setprecision(1) << std::setw(14) << packets_per_second << " pkt/s"
            << std::setw(14) << nanoseconds_per_packet << " ns/pkt";
  if (payload_bytes != 0) {
    const double mib_per_second =
        packets_per_second * static_cast<double>(payload_bytes) / (1024.0 * 1024.0);
    std::cout << std::setw(12) << mib_per_second << " MiB/s";
  }
  std::cout << '\n';
}

}  // namespace

int main(int argc, char** argv) {
  try {
    const std::size_t iterations =
        argc > 1 ? static_cast<std::size_t>(std::stoull(argv[1])) : 10000;
    RunBenchmark(
        "c_api/pass_through/i64", iterations, sizeof(int64_t),
        [](int64_t timestamp) { return lmflow_packet_from_i64(0, timestamp); },
        "PassThroughKernel");

    constexpr int64_t kLargeShape[] = {512, 512, 3};
    LMFlowBuffer large_view{};
    OwnedPacket large(lmflow_packet_new_buffer(3, kLargeShape, LMFLOW_DTYPE_U8, 0, &large_view));
    RunBenchmark(
        "c_api/pass_through/buffer_768k", iterations, 512 * 512 * 3,
        [&large](int64_t timestamp) {
          LMFlowPacket packet = lmflow_packet_clone(&large.packet);
          packet.timestamp = timestamp;
          return packet;
        },
        "PassThroughKernel");

    constexpr int64_t kSmallShape[] = {16, 16};
    LMFlowBuffer small_view{};
    OwnedPacket small(lmflow_packet_new_buffer(2, kSmallShape, LMFLOW_DTYPE_U8, 0, &small_view));
    RunBenchmark(
        "c_api/invert/buffer_256b", iterations, 16 * 16,
        [&small](int64_t timestamp) {
          LMFlowPacket packet = lmflow_packet_clone(&small.packet);
          packet.timestamp = timestamp;
          return packet;
        },
        "InvertKernel");

    const std::size_t large_iterations = iterations > 100 ? iterations / 100 : 1;
    RunBenchmark(
        "c_api/invert/buffer_768k", large_iterations, 512 * 512 * 3,
        [&large](int64_t timestamp) {
          LMFlowPacket packet = lmflow_packet_clone(&large.packet);
          packet.timestamp = timestamp;
          return packet;
        },
        "InvertKernel");
  } catch (const std::exception& error) {
    std::cerr << "benchmark failed: " << error.what() << '\n';
    return EXIT_FAILURE;
  }
  return EXIT_SUCCESS;
}
