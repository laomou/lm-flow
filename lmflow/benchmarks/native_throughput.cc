#include "lmflow/flow.h"

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iomanip>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

namespace {

using Clock = std::chrono::steady_clock;

LMFlowStatus BufferAllocProcess(void*, LMFlowContext* context) {
  constexpr int64_t kShape[] = {3072, 4096, 3};
  constexpr std::size_t kBytes = 3072 * 4096 * 3 * 2;
  LMFlowBuffer view{};
  LMFlowPacket packet =
      lmflow_packet_new_buffer_uninit(3, kShape, LMFLOW_DTYPE_F16, 0, &view);
  if (packet.payload == nullptr) {
    lmflow_ctx_set_error(context, lmflow_last_error());
    return LMFLOW_ERR_KERNEL;
  }
  std::memset(view.data, 0x5a, kBytes);
  lmflow_packet_drop(&packet);
  return LMFLOW_OK;
}

void BufferAllocContract(void*, LMFlowContract* contract) {
  for (std::size_t index = 0; index < lmflow_contract_num_inputs(contract); ++index) {
    lmflow_contract_input_set_any(contract, index);
  }
  for (std::size_t index = 0; index < lmflow_contract_num_outputs(contract); ++index) {
    lmflow_contract_output_set_any(contract, index);
  }
}

void RegisterBufferAllocKernel() {
  static const LMFlowKernelVTable vtable{
      .create = nullptr,
      .get_contract = BufferAllocContract,
      .open = nullptr,
      .process = BufferAllocProcess,
      .close = nullptr,
      .destroy = nullptr,
  };
  static const bool registered =
      lmflow_register_kernel_with_language("BufferAllocKernel", &vtable, nullptr,
                                           LMFLOW_KERNEL_LANGUAGE_C) == LMFLOW_OK;
  if (!registered) {
    throw std::runtime_error(std::string("register BufferAllocKernel: ") +
                             lmflow_last_error());
  }
}

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

Result RunKernelAllocationBenchmark(std::size_t iterations, std::size_t pool_bytes) {
  RegisterBufferAllocKernel();
  const std::string yaml =
      "buffer_pool_max_bytes: " + std::to_string(pool_bytes) +
      "\n"
      "nodes:\n"
      "  - { name: \"allocator\", kernel: \"BufferAllocKernel\", input_ports: [\"in\"] }\n"
      "input_ports: [\"in\"]\n";
  LMFlowGraph* graph = lmflow_graph_new();
  if (graph == nullptr) {
    throw std::runtime_error(lmflow_last_error());
  }
  if (lmflow_graph_init_from_yaml(graph, yaml.c_str()) != LMFLOW_OK ||
      lmflow_graph_start(graph) != LMFLOW_OK) {
    const std::string error = lmflow_last_error();
    lmflow_graph_free(graph);
    throw std::runtime_error("buffer allocation graph: " + error);
  }
  LMFlowInput* input = lmflow_graph_input(graph, "in");
  if (input == nullptr) {
    const std::string error = lmflow_last_error();
    lmflow_graph_free(graph);
    throw std::runtime_error("buffer allocation input: " + error);
  }
  const std::size_t warmup = iterations / 10 + 1;
  for (std::size_t index = 0; index < warmup; ++index) {
    Check(lmflow_input_send(input, lmflow_packet_from_i64(0, static_cast<int64_t>(index))),
          "buffer allocation warmup");
    Check(lmflow_graph_wait_until_idle(graph), "buffer allocation warmup idle");
  }
  const auto begin = Clock::now();
  for (std::size_t index = 0; index < iterations; ++index) {
    Check(lmflow_input_send(
              input, lmflow_packet_from_i64(0, static_cast<int64_t>(warmup + index))),
          "buffer allocation send");
    Check(lmflow_graph_wait_until_idle(graph), "buffer allocation idle");
  }
  const auto elapsed = Clock::now() - begin;
  lmflow_graph_close_all_inputs(graph);
  Check(lmflow_graph_wait_done(graph), "buffer allocation done");
  lmflow_input_free(input);
  lmflow_graph_free(graph);
  const double seconds = std::chrono::duration<double>(elapsed).count();
  constexpr std::size_t kBytes = 3072 * 4096 * 3 * 2;
  const std::string name =
      pool_bytes == 0
          ? "c_api/callback_allocation_pool_off"
          : "c_api/callback_allocation_pool_" + std::to_string(pool_bytes / (1024 * 1024)) + "m";
  return {name,
          iterations, kBytes, static_cast<double>(iterations) / seconds,
          seconds * 1e9 / static_cast<double>(iterations)};
}

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
    results.push_back(RunKernelAllocationBenchmark(std::max<std::size_t>(1, iterations / 100), 0));
    results.push_back(
        RunKernelAllocationBenchmark(std::max<std::size_t>(1, iterations / 100), 64 * 1024 * 1024));
    results.push_back(
        RunKernelAllocationBenchmark(std::max<std::size_t>(1, iterations / 100), 256 * 1024 * 1024));
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
