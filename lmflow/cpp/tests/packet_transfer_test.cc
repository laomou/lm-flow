// Exercise ownership against the real engine, including borrowed Context::Input.
#include <cstdio>
#include <cstdlib>
#include "lmflow/flow.hpp"

static void require(bool condition) {
  if (!condition) {
    std::fputs("packet transfer assertion failed\n", stderr);
    // On the old implementation owners may already dangle. Do not destruct them.
    std::_Exit(1);
  }
}
struct BorrowedFanout : lmflow::Kernel {
  lmflow::Status Process(lmflow::Context& cc) override {
    auto borrowed = cc.Input(0);
    LMFlowBuffer buffer{};
    require(borrowed.SetMetadata("key", int64_t{1}) == LMFLOW_ERR_INVALID_ARG);
    require(borrowed.SetMetadata("key", 1.0) == LMFLOW_ERR_INVALID_ARG);
    require(borrowed.SetMetadata("key", true) == LMFLOW_ERR_INVALID_ARG);
    require(borrowed.SetMetadata("key", "value") == LMFLOW_ERR_INVALID_ARG);
    require(!borrowed.RemoveMetadata("key"));
    require(borrowed.MakeMutableBuffer(&buffer) == LMFLOW_ERR_INVALID_ARG);
    void* data = nullptr;
    size_t size = 0;
    require(borrowed.MakeMutableBytes(&data, &size) == LMFLOW_ERR_INVALID_ARG);
    cc.Emit(0, cc.Input(0));
    cc.Emit(1, cc.Input(0).At(8));
    return lmflow::Status::Ok();
  }
};
int main() {
  require(lmflow_register_kernel("TransferFanout", lmflow::KernelAdapter<BorrowedFanout>::vtable(), nullptr) == LMFLOW_OK);
  unsigned char bytes[] = {1, 2, 3, 4};
  int releases = 0;
  LMFlowBuffer buffer{};
  buffer.data = bytes; buffer.ndim = 1; buffer.shape[0] = 4;
  buffer.strides[0] = 1; buffer.dtype = LMFLOW_DTYPE_U8;
  {
    lmflow::KernelRunner runner("TransferFanout", 1, 2);
    auto input = lmflow::Packet::AdoptBuffer(buffer, [](void* p) { ++*static_cast<int*>(p); }, &releases);
    require(input.SetMetadata("key", int64_t{7}) == LMFLOW_OK);
    require(runner.add_input(0, std::move(input).At(5)).ok());
    require(input.IsEmpty());
    require(runner.process(5).ok());
    auto first = runner.try_next(0);
    auto second = runner.try_next(1);
    require(runner.close().ok());
    require(releases == 0 && first && second);
    require(first->Timestamp() == 5 && second->Timestamp() == 8);
    int64_t metadata = 0;
    require(first->Metadata("key", &metadata) && metadata == 7);
    require(first->AsBuffer(&buffer) && static_cast<unsigned char*>(buffer.data)[3] == 4);
    first.reset();
    require(releases == 0);
    second.reset();
    require(releases == 1);
  }
  auto owned = lmflow::Packet::FromI64(42);
  auto raw = owned.release();
  require(owned.IsEmpty());
  auto again = owned.release();
  require(!again.owner && !again.payload && !again.drop_fn);
  lmflow_packet_drop(&again);
  lmflow_packet_drop(&raw);
  auto source = lmflow::Packet::FromI64(7);
  auto moved = std::move(source);
  raw = source.release();
  require(!raw.owner && !raw.payload);
  require(source.SetMetadata("x", true) == LMFLOW_ERR_INVALID_ARG);
  int64_t value = 0;
  require(moved.AsI64(&value) && value == 7);
  auto local = lmflow::Packet::Make<int>(3);
  raw = local.release();
  require(local.IsEmpty() && !local.release().drop_fn);
  lmflow_packet_drop(&raw);
  std::puts("packet transfer tests passed");
}
