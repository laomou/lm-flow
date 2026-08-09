#include "lmflow/flow.h"

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <iomanip>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

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

struct Result {
  std::string name;
  std::size_t iterations;
  std::size_t payload_bytes;
  double packets_per_second;
  double nanoseconds_per_packet;
};

template <typename MakePacket>
Result RunBenchmark(const char* name, std::size_t iterations, std::size_t payload_bytes,
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

  return {name, iterations, payload_bytes, packets_per_second, nanoseconds_per_packet};
}

Result RunAllocationBenchmark(std::size_t iterations) {
  constexpr int64_t kShape[] = {3072, 4096, 3};
  constexpr std::size_t kBytes = 3072 * 4096 * 3 * 2;
  const auto begin = Clock::now();
  for (std::size_t index = 0; index < iterations; ++index) {
    LMFlowBuffer view{};
    OwnedPacket packet(
        lmflow_packet_new_buffer(3, kShape, LMFLOW_DTYPE_F16, static_cast<int64_t>(index), &view));
    if (packet.packet.payload == nullptr) {
      throw std::runtime_error(std::string("new_buffer: ") + lmflow_last_error());
    }
  }
  const auto elapsed = Clock::now() - begin;
  const double seconds = std::chrono::duration<double>(elapsed).count();
  const double packets_per_second = static_cast<double>(iterations) / seconds;
  return {"c_api/allocation/3072x4096x3_f16", iterations, kBytes, packets_per_second,
          seconds * 1e9 / static_cast<double>(iterations)};
}

}  // namespace

int main(int argc, char** argv) {
  try {
    std::size_t iterations = 10000;
    bool json = false;
    for (int index = 1; index < argc; ++index) {
      const std::string argument = argv[index];
      if (argument == "--json") {
        json = true;
      } else {
        iterations = static_cast<std::size_t>(std::stoull(argument));
      }
    }
    std::vector<Result> results;
    results.push_back(RunAllocationBenchmark(std::max<std::size_t>(1, iterations / 100)));
    results.push_back(RunBenchmark(
        "c_api/pass_through/i64", iterations, sizeof(int64_t),
        [](int64_t timestamp) { return lmflow_packet_from_i64(0, timestamp); },
        "PassThroughKernel"));

    constexpr int64_t kLargeShape[] = {512, 512, 3};
    LMFlowBuffer large_view{};
    OwnedPacket large(lmflow_packet_new_buffer(3, kLargeShape, LMFLOW_DTYPE_U8, 0, &large_view));
    results.push_back(RunBenchmark(
        "c_api/pass_through/buffer_768k", iterations, 512 * 512 * 3,
        [&large](int64_t timestamp) {
          LMFlowPacket packet = lmflow_packet_clone(&large.packet);
          packet.timestamp = timestamp;
          return packet;
        },
        "PassThroughKernel"));

    constexpr int64_t kSmallShape[] = {16, 16};
    LMFlowBuffer small_view{};
    OwnedPacket small(lmflow_packet_new_buffer(2, kSmallShape, LMFLOW_DTYPE_U8, 0, &small_view));
    results.push_back(RunBenchmark(
        "c_api/invert/buffer_256b", iterations, 16 * 16,
        [&small](int64_t timestamp) {
          LMFlowPacket packet = lmflow_packet_clone(&small.packet);
          packet.timestamp = timestamp;
          return packet;
        },
        "InvertKernel"));

    const std::size_t large_iterations = iterations > 100 ? iterations / 100 : 1;
    results.push_back(RunBenchmark(
        "c_api/invert/buffer_768k", large_iterations, 512 * 512 * 3,
        [&large](int64_t timestamp) {
          LMFlowPacket packet = lmflow_packet_clone(&large.packet);
          packet.timestamp = timestamp;
          return packet;
        },
        "InvertKernel"));
    if (json) {
      std::cout << "{\"language\":\"cpp\",\"results\":[";
      for (std::size_t index = 0; index < results.size(); ++index) {
        if (index != 0) std::cout << ',';
        const auto& result = results[index];
        std::cout << "{\"name\":\"" << result.name << "\",\"iterations\":"
                  << result.iterations << ",\"payload_bytes\":" << result.payload_bytes
                  << ",\"packets_per_second\":" << result.packets_per_second
                  << ",\"nanoseconds_per_packet\":" << result.nanoseconds_per_packet;
        if (result.payload_bytes != 0) {
          std::cout << ",\"mib_per_second\":"
                    << result.packets_per_second * static_cast<double>(result.payload_bytes) /
                           (1024.0 * 1024.0);
        }
        std::cout << '}';
      }
      std::cout << "]}\n";
    } else {
      for (const auto& result : results) {
        std::cout << std::left << std::setw(34) << result.name << std::right << std::fixed
                  << std::setprecision(1) << std::setw(14) << result.packets_per_second
                  << " pkt/s" << std::setw(14) << result.nanoseconds_per_packet << " ns/pkt";
        if (result.payload_bytes != 0) {
          std::cout << std::setw(12)
                    << result.packets_per_second * static_cast<double>(result.payload_bytes) /
                           (1024.0 * 1024.0)
                    << " MiB/s";
        }
        std::cout << '\n';
      }
    }
  } catch (const std::exception& error) {
    std::cerr << "benchmark failed: " << error.what() << '\n';
    return EXIT_FAILURE;
  }
  return EXIT_SUCCESS;
}
