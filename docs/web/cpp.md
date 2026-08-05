# C / C++ guide

The engine exposes exactly one stable interface: the **C ABI** declared in
[`lmflow/flow.h`](https://github.com/laomou/lm-flow/blob/main/lmflow/include/lmflow/flow.h). Everything
else is optional convenience layered on top of it:

| Header | Purpose | Part of the ABI? |
|---|---|---|
| [`flow.h`](https://github.com/laomou/lm-flow/blob/main/lmflow/include/lmflow/flow.h) | The C ABI — graphs, packets, kernels, contexts. Pure C, includable from C and C++. | **Yes — the only stable interface** |
| [`flow.hpp`](https://github.com/laomou/lm-flow/blob/main/lmflow/include/lmflow/flow.hpp) | Header-only C++ sugar for writing kernels: `lmflow::Kernel`, `Packet`, `Context`, `Contract`. Zero runtime overhead — it is built entirely on `flow.h`. | No |
| [`flow_cv.hpp`](https://github.com/laomou/lm-flow/blob/main/lmflow/include/lmflow/flow_cv.hpp) | `LMFlowBuffer` ↔ `cv::Mat` interop. Include it only if you use OpenCV. | No |
| [`flow_platform_log.hpp`](https://github.com/laomou/lm-flow/blob/main/lmflow/include/lmflow/flow_platform_log.hpp) | Bridge engine logs to logcat / os_log / HiLog in one call. | No |

You can write both hosts and kernels against raw `flow.h` if you prefer; `flow.hpp` exists so you
do not have to hand-write function-pointer vtables.

## Getting the library

Each tagged release ships a per-platform SDK tarball — Linux x86_64 / aarch64, macOS arm64,
iOS arm64, Android arm64:

```text
lmflow-v0.3.0-linux-x86_64/
├── include/lmflow/   flow.h · flow.hpp · flow_cv.hpp · flow_platform_log.hpp
└── lib/              liblmflow.a (static, self-contained — preferred) · liblmflow.so
```

The static library is self-contained and is the right choice for mobile embedding:

```bash
g++ -std=c++17 -Iinclude my_host.cc lib/liblmflow.a -lpthread -ldl -lm -o my_host
```

### With CMake

Building from the repository installs a `find_package` config:

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
ctest --test-dir build
cmake --install build --prefix /opt/lmflow
```

Consumers then need only:

```cmake
find_package(lmflow REQUIRED)
target_link_libraries(my_app PRIVATE lmflow::core)   # headers + liblmflow.a + system libs
```

The imported target carries the include directory, the static library, and the system libraries it
needs, so nothing else has to be spelled out.

### Building the library directly

```bash
cd lmflow/core
cargo build --release --features builtin-kernels
# → lmflow/core/target/release/liblmflow.{a,so}; headers are in lmflow/include/lmflow
```

`builtin-kernels` is **off by default** — the default build is a pure-Rust engine with no C++ at
all. C and C++ users generally want the feature on, since it is what provides the bundled kernels
and `lmflow_register_builtin_kernels()`. The released SDK tarballs and the Python wheel are all
built with it enabled.

## ABI version checking

```c
#if 0 /* illustrative */
#define LMFLOW_ABI_VERSION 2u
#endif
uint32_t lmflow_abi_version(void);
```

When you **dynamically** link, a header/library version mismatch silently corrupts struct layouts.
Check it at startup:

```c
if (lmflow_abi_version() != LMFLOW_ABI_VERSION) {
  fprintf(stderr, "ABI mismatch: lib=%u header=%u\n", lmflow_abi_version(), LMFLOW_ABI_VERSION);
  return 1;
}
```

`lmflow_graph_new()` also verifies it internally and returns `NULL` on mismatch.

## Conventions

Four rules govern the whole ABI:

1. Functions returning `LMFlowStatus` return `0` (`LMFLOW_OK`) on success.
2. A `const char*` you pass **in** is only read for the duration of the call; the engine copies
   whatever it needs to keep.
3. A `const char*` returned **to** you is owned by the engine. Never `free` it. Lifetimes are noted
   per function — most are valid as long as the graph is.
4. `LMFlowContext*` and `LMFlowContract*` are valid **only inside the callback that received them**.
   Never store them.

### Status codes

| Code | Value | Meaning |
|---|---|---|
| `LMFLOW_OK` | 0 | success |
| `LMFLOW_ERR_INVALID_ARG` | 1 | illegal config or argument (undefined port name, invalid topology) |
| `LMFLOW_ERR_NOT_FOUND` | 2 | name lookup failed (kernel not registered, no such port) |
| `LMFLOW_ERR_KERNEL` | 3 | a kernel callback returned failure or threw |
| `LMFLOW_ERR_PANIC` | 4 | caught by the engine's `catch_unwind` backstop |
| `LMFLOW_ERR_WOULD_BLOCK` | 5 | non-blocking call: queue full, or nothing available |
| `LMFLOW_ERR_TIMEOUT` | 6 | a call with a timeout timed out |
| `LMFLOW_ERR_CANCELLED` | 7 | the graph was cancelled |
| `LMFLOW_ERR_CLOSED` | 8 | port already closed, or graph already terminated |
| `LMFLOW_ERR_ABI` | 9 | ABI version mismatch |
| `LMFLOW_ERR_UNSUPPORTED` | 10 | the config used a feature this version does not implement |
| `LMFLOW_ERR_STATE` | 11 | the graph's state forbids this operation (start twice, send before start) |

`LMFLOW_ERR_UNSUPPORTED` is deliberate: configuration asking for something unimplemented is
**rejected loudly** instead of silently ignored. Quietly doing less than the config asked for is the
one failure mode you cannot debug from the outside.

```c
const char* lmflow_last_error(void);              /* thread-local */
const char* lmflow_graph_last_error(LMFlowGraph*); /* per graph */
```

`lmflow_last_error()` is **thread-local and only valid until this thread's next `lmflow_*` call**.
Read it only after you have seen a non-zero status or a `NULL` handle — the success path does not
promise to clear it.

## Host lifecycle

The order matters in two places: kernels must be registered before `init`, and pollers must be
attached before `start`.

```c
#include <cstdio>
#include "lmflow/flow.h"

static const char* kConfig = R"(
nodes:
  - name: "node1"
    kernel: "PassThroughKernel"
    input_ports: ["input1"]
    output_ports: ["mid"]
  - name: "node2"
    kernel: "PassThroughKernel"
    input_ports: ["mid"]
    output_ports: ["output2"]
input_ports: ["input1"]
output_ports: ["output2"]
)";

#define CHECK(expr)                                                                \
  do {                                                                             \
    LMFlowStatus st_ = (expr);                                                     \
    if (st_ != LMFLOW_OK) {                                                        \
      fprintf(stderr, "%s failed: %d (%s)\n", #expr, st_, lmflow_last_error());    \
      return 1;                                                                    \
    }                                                                              \
  } while (0)

int main(void) {
  if (lmflow_abi_version() != LMFLOW_ABI_VERSION) return 1;

  lmflow_register_builtin_kernels();          /* before init, or "kernel not registered" */

  LMFlowGraph* graph = lmflow_graph_new();
  if (!graph) { fprintf(stderr, "%s\n", lmflow_last_error()); return 1; }

  CHECK(lmflow_graph_init_from_yaml(graph, kConfig));

  LMFlowPoller* poller = lmflow_graph_add_poller(graph, "output2");  /* before start */
  if (!poller) { fprintf(stderr, "%s\n", lmflow_last_error()); return 1; }

  CHECK(lmflow_graph_start(graph));

  LMFlowInput* in = lmflow_graph_input(graph, "input1");   /* handle: no per-packet lookup */

  for (int i = 0; i < 10; ++i) {
    CHECK(lmflow_input_send(in, lmflow_packet_from_i64(i, i)));

    LMFlowPacket out;
    if (!lmflow_poller_next(poller, &out)) break;          /* false = stream ended */
    int64_t v = 0;
    if (lmflow_packet_as_i64(&out, &v)) printf("out: %lld\n", (long long)v);
    lmflow_packet_drop(&out);                              /* transferred to you — must drop */
  }

  lmflow_graph_close_all_inputs(graph);
  CHECK(lmflow_graph_wait_done(graph));

  lmflow_input_free(in);
  lmflow_poller_free(poller);
  lmflow_graph_free(graph);
  return 0;
}
```

Construction and control:

```c
LMFlowGraph* lmflow_graph_new(void);
LMFlowStatus lmflow_graph_init_from_yaml(LMFlowGraph*, const char* yaml);
LMFlowStatus lmflow_graph_init_from_yaml_file(LMFlowGraph*, const char* path);
LMFlowStatus lmflow_graph_start(LMFlowGraph*);
LMFlowStatus lmflow_graph_reset(LMFlowGraph*);
void         lmflow_graph_free(LMFlowGraph*);
```

`init_from_yaml_file` is the one that supports `include:`, because relative paths can only be
resolved against a real file.

Feeding input — the handle form avoids a name lookup per packet, and the handle lives as long as
the graph:

```c
LMFlowInput*  lmflow_graph_input(LMFlowGraph*, const char* port);
LMFlowStatus  lmflow_input_send(LMFlowInput*, LMFlowPacket pkt);      /* blocks at the watermark */
LMFlowStatus  lmflow_input_try_send(LMFlowInput*, LMFlowPacket pkt);  /* WOULD_BLOCK if full */
void          lmflow_input_close(LMFlowInput*);
void          lmflow_input_free(LMFlowInput*);

LMFlowStatus  lmflow_graph_add_packet(LMFlowGraph*, const char* port, LMFlowPacket pkt); /* by name */
LMFlowStatus  lmflow_graph_close_input(LMFlowGraph*, const char* port);
void          lmflow_graph_close_all_inputs(LMFlowGraph*);
```

Graph input ports are the **only** place the engine applies back-pressure. Internal edges are
unbounded by design: putting a hard bound on them would deadlock a legitimate fan-out-then-join
DAG. Memory is instead constrained by the `fixed_size` input policy and the global watermarks.

Termination is: close the inputs, then wait for the pipeline to drain.

```c
void         lmflow_graph_cancel(LMFlowGraph*);
LMFlowStatus lmflow_graph_wait_done(LMFlowGraph*);
LMFlowStatus lmflow_graph_wait_done_timeout(LMFlowGraph*, int64_t timeout_ms);
LMFlowStatus lmflow_graph_wait_until_idle(LMFlowGraph*);
LMFlowStatus lmflow_graph_wait_until_idle_timeout(LMFlowGraph*, int64_t timeout_ms);
void         lmflow_graph_pause(LMFlowGraph*);
void         lmflow_graph_resume(LMFlowGraph*);
```

`cancel` is cooperative. The engine has no way to preempt a kernel that is already running — see
[Observability](#observability) for how to find out which node is stuck.

### Taking output: pull or push

```c
/* pull */
LMFlowPoller* lmflow_graph_add_poller(LMFlowGraph*, const char* port);
LMFlowPoller* lmflow_graph_add_poller_ex(LMFlowGraph*, const char* port, bool observe_timestamp_bounds);
LMFlowPoller* lmflow_graph_add_poller_bounded(
    LMFlowGraph*, const char* port, size_t capacity, int overflow_policy);
bool          lmflow_poller_next(LMFlowPoller*, LMFlowPacket* out);       /* blocking */
bool          lmflow_poller_try_next(LMFlowPoller*, LMFlowPacket* out);   /* non-blocking */
LMFlowStatus  lmflow_poller_next_timeout(LMFlowPoller*, LMFlowPacket* out, int64_t timeout_ms);
uint64_t      lmflow_poller_dropped_count(LMFlowPoller*);
void          lmflow_poller_free(LMFlowPoller*);

/* push */
LMFlowStatus lmflow_graph_observe(LMFlowGraph*, const char* port,
                                  void (*cb)(void* user, LMFlowPacket pkt), void* user);
LMFlowStatus lmflow_graph_observe_ex(LMFlowGraph*, const char* port, bool observe_timestamp_bounds,
                                     void (*cb)(void* user, LMFlowPacket pkt), void* user);
```

A packet from a poller is **transferred** to you — you must `lmflow_packet_drop` it. A packet handed
to an observer callback is **borrowed** — you must not. The observer runs on whichever thread
dispatched the packet, possibly a pool thread, so it must be thread-safe; and it must not call back
into `lmflow_graph_*`.

Poller queues contribute to the graph's packet watermark and packet/byte diagnostic counters. The legacy
`lmflow_graph_add_poller` remains unbounded for compatibility. A bounded Poller supports:

| Policy | Behavior at capacity |
|---|---|
| `LMFLOW_POLLER_BLOCK` | lossless; waits until another thread drains the Poller |
| `LMFLOW_POLLER_DROP_OLDEST` | drops the oldest queued packet |
| `LMFLOW_POLLER_DROP_NEWEST` | rejects the incoming packet |
| `LMFLOW_POLLER_LATEST` | capacity must be 1; retains only the newest packet |

Lossy policies increment `lmflow_poller_dropped_count`. Freeing a Poller unregisters its
subscription and releases any packets still queued in it.

A node that declares no `executor` runs on the **default executor** — a thread pool sized to the
CPU count, which the engine creates for you. That is why the example above makes progress with no
`executors:` block at all.

If you want the older behaviour (zero concurrency, deterministic order, Python kernels free of GIL
contention), make the default executor hand work back to the host thread:

```yaml
executors:
  - { name: "", type: "DelegatingExecutor" }   # empty name configures the default
```

A delegating executor owns no threads, so its tasks are only pumped while the host is inside a
blocking call such as `lmflow_poller_next` or `lmflow_graph_wait_done` (or an explicit
`pump_step`). Send without ever entering the engine and those nodes will not advance.

### Reset and re-run

```c
LMFlowStatus lmflow_graph_reset(LMFlowGraph*);
```

After a graph has terminated, `reset` returns it to a startable state while **keeping already-opened
kernel instances alive** — so a one-off cost such as loading a model is not paid again on the next
run. It requires the graph to be terminated and idle; calling it on a running graph returns
`LMFLOW_ERR_STATE`.

## Packets and ownership

```c
typedef struct {
  void* payload;                  /* the data; treated as immutable and shared */
  uint64_t type_id;               /* type tag */
  int64_t timestamp;
  void* owner;                    /* engine-internal reference handle; NULL = unowned */
  void (*drop_fn)(void* payload); /* engine calls this once when the refcount hits zero */
} LMFlowPacket;
```

**This is the section that prevents leaks and double frees.** A `LMFlowPacket` reaches you in one of
three forms, and what you must do differs in each:

| Form | Where it comes from | Your obligation |
|---|---|---|
| **Host-created** (`owner == NULL`) | you filled the struct in yourself, or a `lmflow_packet_from_*` constructor | the engine adopts it when you submit it; if you never submit it, `lmflow_packet_drop` it |
| **Borrowed** | `lmflow_ctx_input`, `lmflow_ctx_input_at`, an observer callback argument, `lmflow_ctx_side_packet` | **never** drop it, and never let it outlive the callback |
| **Transferred** | `lmflow_poller_next`, `_try_next`, `_next_timeout`, `lmflow_ctx_take_input` | you **must** `lmflow_packet_drop` it, or it leaks |

```c
void lmflow_packet_drop(LMFlowPacket* pkt);
```

`drop` handles both owned forms correctly: with `owner != NULL` it returns the engine reference (the
engine calls `drop_fn` when the count reaches zero); with `owner == NULL` it calls `drop_fn`
directly. It zeroes the struct afterwards, so calling it twice is safe.

A packet with `payload == NULL` is an empty packet — legal, and used to carry a pure timestamp.

Constructing a packet by hand is a matter of filling the five fields:

```c
static LMFlowPacket MakeInt(int value, int64_t ts) {
  LMFlowPacket p;
  p.payload = new int(value);
  p.type_id = 0;                                        /* 0 = don't declare a type */
  p.timestamp = ts;
  p.owner = nullptr;                                    /* host-created */
  p.drop_fn = [](void* q) { delete static_cast<int*>(q); };
  return p;
}
```

### Built-in payload types

These exist so packets can cross a language boundary. The engine gives them **no privileges** — it
still never interprets a payload; they are simply a memory convention both sides understand.

| Constant | Value | Payload |
|---|---|---|
| `LMFLOW_TYPE_NONE` | 0 | no type declared, checking skipped |
| `LMFLOW_TYPE_BYTES` | 1 | one-dimensional byte block |
| `LMFLOW_TYPE_I64` | 2 | `int64_t` |
| `LMFLOW_TYPE_F64` | 3 | `double` |
| `LMFLOW_TYPE_BOOL` | 4 | `bool` |
| `LMFLOW_TYPE_STR` | 5 | NUL-terminated UTF-8 |
| `LMFLOW_TYPE_BUFFER` | 6 | N-dimensional strided buffer, see `LMFlowBuffer` |
| `LMFLOW_TYPE_HOST_OBJECT` | 7 | reserved, **not enabled** in this version |

```c
LMFlowPacket lmflow_packet_from_bytes(const void* data, size_t len, int64_t ts);
LMFlowPacket lmflow_packet_from_i64(int64_t value, int64_t ts);
LMFlowPacket lmflow_packet_from_f64(double value, int64_t ts);
LMFlowPacket lmflow_packet_from_bool(bool value, int64_t ts);
LMFlowPacket lmflow_packet_from_str(const char* utf8, int64_t ts);

bool lmflow_packet_as_bytes(const LMFlowPacket*, const void** data, size_t* len);
bool lmflow_packet_as_i64(const LMFlowPacket*, int64_t* out);
bool lmflow_packet_as_f64(const LMFlowPacket*, double* out);
bool lmflow_packet_as_bool(const LMFlowPacket*, bool* out);
bool lmflow_packet_as_str(const LMFlowPacket*, const char** out);
```

The `as_*` accessors return `false` on a type mismatch or an empty packet. Returned pointers stay
valid as long as the packet does.

**Why cross-language kernels are restricted to these.** An arbitrary C++ object is an opaque pointer
to a Python or Rust kernel — it cannot be read, and the mistake only shows up at runtime, in a graph
that type-checked fine. A C++ kernel may pass any registered type to another C++ kernel; but the
moment a payload might cross into another language, use a built-in type — `BUFFER` for arrays and
images, or `STR` carrying JSON for structured data.

### Buffers

```c
#define LMFLOW_MAX_DIMS 8

typedef struct {
  void* data;
  int64_t shape[LMFLOW_MAX_DIMS];
  int64_t strides[LMFLOW_MAX_DIMS];   /* in BYTES */
  int32_t ndim;                       /* 1..LMFLOW_MAX_DIMS */
  int32_t dtype;                      /* LMFLOW_DTYPE_* */
  uint32_t flags;                     /* LMFLOW_BUF_FLAG_* */
  int32_t device;                     /* LMFLOW_DEVICE_CPU */
  int64_t reserved[2];                /* zero it */
} LMFlowBuffer;
```

One N-dimensional descriptor covers images, tensors and audio, with semantics aligned to the numpy
buffer protocol — rather than separate IMAGE / TENSOR / AUDIO types.

dtypes: `U8`=0, `I8`=1, `U16`=2, `I16`=3, `I32`=4, `I64`=5, `F16`=6, `F32`=7, `F64`=8, with
`size_t lmflow_dtype_size(int32_t dtype)` returning 0 for an unknown value.
`LMFLOW_BUF_FLAG_READONLY` is set on views obtained from `lmflow_packet_as_buffer`.

**Always zero the struct before filling it** (`LMFlowBuffer b{};` in C++, `memset` in C). The
`reserved` fields are a one-time allowance for future growth — most likely describing non-CPU memory
— so that adding a field does not change `sizeof` and does not break existing binaries.

```c
LMFlowPacket lmflow_packet_new_buffer(int32_t ndim, const int64_t* shape, int32_t dtype,
                                      int64_t ts, LMFlowBuffer* out);
LMFlowPacket lmflow_packet_from_buffer(const LMFlowBuffer* src, int64_t ts); /* copies */
bool         lmflow_packet_as_buffer(const LMFlowPacket*, LMFlowBuffer* out); /* read-only view */
```

### Copy-on-write

```c
LMFlowPacket lmflow_packet_clone(const LMFlowPacket* pkt);   /* refcount++, no data copy */
LMFlowStatus lmflow_packet_make_mutable_buffer(LMFlowPacket* pkt, LMFlowBuffer* out);
LMFlowStatus lmflow_packet_make_mutable_bytes(LMFlowPacket* pkt, void** data, size_t* len);
```

Payloads are immutable and shared; mutation goes through copy-on-write. **The rule that makes it
actually zero-copy: take the input first, then make it mutable.**

```cpp
lmflow::Packet p = cc.TakeInput(0);      // now the sole reference
LMFlowBuffer buf{};
if (p.MakeMutableBuffer(&buf) != LMFLOW_OK) return cc.Fail("input 0 is not a buffer");
// ... write through buf.data in place, no copy ...
cc.Emit(0, std::move(p));
```

If you merely *borrow* the input with `Input(0)`, the context still holds a reference, the packet is
shared, and `MakeMutable*` is obliged to copy. In a linear pipeline `TakeInput` keeps the whole chain
copy-free; when a packet is genuinely fanned out to several consumers, the copy happens then — which
is exactly what stops one branch from corrupting another.

## Writing a kernel with `flow.hpp`

Implement `Process`; `Open` and `Close` are optional. A static `GetContract` is picked up
automatically if you provide one.

```cpp
#include "lmflow/flow.hpp"

class ScaleKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) {
    c.InputSetBuiltin(0, LMFLOW_TYPE_I64);
    c.OutputSetBuiltin(0, LMFLOW_TYPE_I64);
  }

  lmflow::Status Open(lmflow::Context& cc) override {
    factor_ = cc.OptionI64("factor", 2);
    LMFLOW_RET_CHECK_MSG(cc, factor_ != 0, "factor must be non-zero");
    return lmflow::Status::Ok();
  }

  lmflow::Status Process(lmflow::Context& cc) override {
    int64_t v = 0;
    LMFLOW_RET_CHECK(cc, cc.Input(0).AsI64(&v));
    cc.Emit(0, lmflow::Packet::FromI64(v * factor_).At(cc.InputTimestamp()));
    return lmflow::Status::Ok();
  }

 private:
  int64_t factor_ = 2;
};

LMFLOW_REGISTER_KERNEL(ScaleKernel);   // registers under "ScaleKernel"
```

`LMFLOW_REGISTER_KERNEL_AS(T, "OtherName")` registers under a different name.

> **Static-initialisation caveat.** `LMFLOW_REGISTER_KERNEL` works by declaring a static registrar
> object, and a linker may strip static initialisers out of a *static* library that nothing else
> references. That is precisely why the bundled kernels are also registered explicitly through
> `lmflow_register_builtin_kernels()`. If your own kernels live in a static library and go missing,
> either add an explicit aggregate registration function that the host calls, or link that archive
> with `--whole-archive`.

### `LMFLOW_RET_CHECK` — failing with a reason

Only an `int32_t` crosses the ABI, so the failure *text* has to travel a separate channel
(`lmflow_ctx_set_error`). `return Status::Error()` therefore hands the host a code with no
explanation. These macros bind "fail" and "say why" into a single action:

```cpp
LMFLOW_RET_CHECK(cc, cond);              // "check failed: cond (at file.cc:42)"
LMFLOW_RET_CHECK_MSG(cc, cond, "why");   // "why (check failed: cond at file.cc:42)"
```

Both stamp in the stringified condition plus `__FILE__:__LINE__`, format into a 256-byte stack
buffer (over-long text is safely truncated), and `return cc.Fail(...)`. They can only be used inside
a function returning `lmflow::Status`.

`cc.Fail(msg)` on its own is the manual equivalent: it records the message and returns
`LMFLOW_ERR_KERNEL`.

### Exceptions never cross the boundary

The `flow.hpp` adapter wraps every phase in try/catch and funnels `std::exception::what()` into the
context's error message, mirroring the engine's own `catch_unwind` on the Rust side. Throwing from a
kernel is safe; it becomes `LMFLOW_ERR_KERNEL` with your `what()` text attached. Letting an
exception escape a raw `flow.h` vtable callback, by contrast, is undefined behaviour.

### The sugar classes

**`lmflow::Status`** — `Ok()`, `Error()`, `ok()`, `code()`, and an implicit constructor from
`LMFlowStatus` so `return LMFLOW_OK;` works.

**`lmflow::Packet`** — move-only RAII around `LMFlowPacket`, so the three ownership forms become
type-safe:

| Member | Purpose |
|---|---|
| `Packet::Make<T>(value)` | wrap any C++ type (allocates, sets `type_id` from `T`) |
| `Packet::Borrow(raw)` / `Packet::Adopt(raw)` | wrap a borrowed / transferred raw packet |
| `Get<T>()` / `TryGet<T>()` / `Is<T>()` | typed access; `TryGet` returns `nullptr` on mismatch |
| `IsEmpty()`, `type_id()`, `Timestamp()` | inspection |
| `At(ts)` | set the timestamp; chains on both lvalues and rvalues |
| `FromBytes` / `FromI64` / `FromF64` / `FromBool` / `FromStr` / `NewBuffer` | built-in constructors |
| `AsBytes` / `AsI64` / `AsF64` / `AsBool` / `AsStr` / `AsBuffer` | built-in accessors |
| `Clone()`, `MakeMutableBuffer()`, `MakeMutableBytes()` | refcount and copy-on-write |
| `release()` | hand the raw packet to the engine; this object stops managing it |

**`lmflow::Contract`** — declare what the ports carry, validated at graph-build time:
`NumInputs`/`NumOutputs`, `InputId(tag, index)`, `InputIndex(name)`, `InputName(i)`,
`InputSetAny(i)`, `InputSet<T>(i)`, `InputSetBuiltin(i, LMFLOW_TYPE_I64)`,
`RequireSidePacket(name)`, and the `Output*` counterparts. Declaring nothing means the port accepts
any type.

`RequireSidePacket` is worth singling out: a missing side packet becomes an **init-time** error
naming the key, rather than a null dereference on the first frame.

**`lmflow::Kernel`** — `virtual Status Process(Context&) = 0`, plus overridable
`Open(Context&)` and `Close(Context&)`.

## The `Context` API

```cpp
/* inputs */
size_t  NumInputs() const;
Packet  Input(size_t i) const;              // borrowed
Packet  TakeInput(size_t i);                // transferred — needed for zero-copy mutation
size_t  InputCount(size_t i) const;         // batch policy: how many packets this activation
Packet  InputAt(size_t i, size_t k) const;  // batch policy: the k-th
const T* InputPtr<T>(size_t i) const;       // fast path when the type is known
bool    InputIsEmpty(size_t i) const;
bool    InputIsDone(size_t i) const;        // upstream closed and drained
int64_t InputTimestamp() const;             // the aligned timestamp of this activation

/* outputs */
void Emit(size_t i, Packet p);
void Forward(size_t in, size_t out);                  // zero-copy passthrough
void SetNextTimestampBound(size_t i, int64_t bound);  // when producing nothing
void SourceDone() const;                              // source kernels: "I am finished"

/* options (from the node's YAML `options:`) */
bool        HasOption(const char* key) const;
int64_t     OptionI64(const char* key, int64_t def = 0) const;
double      OptionF64(const char* key, double def = 0.0) const;
bool        OptionBool(const char* key, bool def = false) const;
const char* OptionStr(const char* key, const char* def = "") const;
LMFlowStatus RequireOption(const char* key, int64_t* out) const;   // also double/bool/const char*
size_t      OptionCount(const char* key) const;
size_t      OptionArray(const char* key, int64_t* out, size_t cap) const;  // also double/const char*
const char* OptionsJson() const;

/* side packets, diagnostics, identity */
bool   HasSidePacket(const char* name) const;
Packet SidePacket(const char* name) const;            // borrowed
void   Log(LMFlowLogLevel, const char*) const;  void LogInfo(...) const;  void LogWarn(...) const;
void   SetError(const char* msg) const;
Status Fail(const char* msg) const;
void   CounterAdd(const char* name, int64_t delta = 1) const;
LMFlowCloseReason CloseReason() const;                // normal drain / graph error / cancelled
const char* NodeName() const;  const char* KernelName() const;
```

Option keys support dotted paths, so `OptionI64("roi.x")` reads a nested value. The `RequireOption`
overloads fail loudly with the key name instead of silently substituting a default — prefer them
whenever a missing option means the kernel cannot work.

`SetNextTimestampBound` matters more than it looks: a node that consumes a packet and emits nothing
must advance the downstream bound, or consumers keep waiting for data that will never come.

## Ports: names versus tags

Two independent identifiers, deliberately kept apart:

- A **name** belongs to the graph. It is what connects an edge, and exactly one node may produce it.
- A **tag** belongs to the kernel. It expresses what the port is *for* (`VIDEO`, `MASK`), so the
  kernel does not depend on YAML declaration order.

Port declarations take three forms — `"name"`, `"TAG:name"`, `"TAG:index:name"` — and the flat index
is simply declaration order.

```cpp
size_t i = cc.InputId("VIDEO", 0);     // by tag — recommended
size_t j = cc.InputIndex("frames");    // by edge name
// or a raw literal index, if you accept the coupling to declaration order
```

Both lookups return `LMFLOW_INVALID_ID` when there is no match. Note that the flat index follows
**declaration order**, not tag-grouped order — so with a mix of tagged and untagged ports, index 0
really is the first one you wrote.

## Writing a kernel against raw `flow.h`

If you would rather not use `flow.hpp`, fill in a vtable directly:

```c
typedef struct {
  void* (*create)(void* factory);
  void (*get_contract)(void* factory, LMFlowContract* out);
  LMFlowStatus (*open)(void* self, LMFlowContext* ctx);
  LMFlowStatus (*process)(void* self, LMFlowContext* ctx);
  LMFlowStatus (*close)(void* self, LMFlowContext* ctx);
  void (*destroy)(void* self);
} LMFlowKernelVTable;

LMFlowStatus lmflow_register_kernel(const char* name, const LMFlowKernelVTable* vt, void* factory);
```

Only `process` is required; the rest may be `NULL`. The engine copies `*vt` **by value** during
the call, so `vt` may be a stack temporary — it does not need static storage. The `factory`
pointer, by contrast, is retained (passed back to `create`/`get_contract` on every
instantiation), so it must outlive the graph. No exception or panic may cross these callbacks.

Every `Context` and `Contract` method shown above has a plain C counterpart: `lmflow_ctx_input`,
`lmflow_ctx_emit`, `lmflow_ctx_option_i64`, `lmflow_contract_input_set_type`, and so on.

## Custom C++ types

`Packet::Make<T>` derives the type id from `typeid(T).name()` via FNV-1a, rather than from
`typeid(T).hash_code()` — `hash_code` is implementation-defined and is not guaranteed to agree
across shared libraries, which this project needs since C++ kernels and the Python bindings live in
different artifacts.

Mangled names still differ between compiler ABIs (GCC and Clang agree; MSVC does not). When kernels
built by different toolchains must interoperate, pin the identity explicitly:

```cpp
LMFLOW_DECLARE_TYPE_NAME(MyDetection, "myproj.MyDetection");
```

The honest limitation: a custom type only travels within a same-language part of the graph. To cross
into Python or Rust, use `LMFLOW_TYPE_BUFFER`, or `LMFLOW_TYPE_STR` carrying JSON.

## Graph configuration

The topology, the threading and the flow-control policy are all configuration.

```yaml
executors:
  - name: cpu
    type: ThreadPoolExecutor
    num_threads: 4
    affinity: [2, 3]      # optional, Linux/Android, best effort
    priority: 10          # optional, SCHED_FIFO 1..99, needs privileges, best effort

nodes:
  - name: camera
    kernel: MySourceKernel
    executor: cpu         # a source (no inputs) must be on a pool, not a delegating executor
    output_ports: ["frames"]
    rate: 30              # Hz — the engine paces it; no sleep in the kernel

  - name: detect
    kernel: MyDetectorKernel
    executor: cpu
    input_ports: ["VIDEO:frames"]
    output_ports: ["boxes"]
    max_in_flight: 2      # >1 needs an executor with more than one thread
    on_error: skip        # drop the bad frame instead of killing the graph
    input_policy: { type: fixed_size, capacity: 2 }
    options:
      threshold: 0.5
      roi: { x: 8, y: 16 }

input_ports: []
output_ports: ["boxes"]
max_queue_size: 100
max_queued_packets: 500
watchdog_ms: 5000
stats_timing: true
```

Node fields: `name`, `kernel` (or `type` for a subgraph instance), `input_ports`, `output_ports`,
`executor`, `max_in_flight`, `options`, `input_policy`, `input_queues`, `back_edges`, `on_error`,
`rate`.

`input_queues.packets` is the node default. `ports` overrides it by input-port name; an omitted port
inherits the default, while an explicit `0` disables the limit:

```yaml
input_queues:
  packets: 8
  ports:
    video: { packets: 2 }
    metadata: { packets: 32 }
    control: { packets: 0 }
```

A full queue pauses the producer with its completed output retained, but releases the executor
thread; dequeue resumes the pending flush. Do not combine `input_queues` limits with lossy
`input_policy: fixed_size`. Queue byte counters remain available for diagnostics, but bytes do not
participate in capacity enforcement.

Graph fields: `executors`, `nodes`, `subgraphs`, `include`, `input_ports`, `output_ports`,
`max_queue_size`, `max_queued_packets`, `watchdog_ms`, `stats_timing`.

### Input policies

| `type` | Behaviour |
|---|---|
| `sync` | default — fire when every input port has a packet, aligned by timestamp |
| `immediate` | each input port fires independently, waiting for no one |
| `fixed_size` | bounded, **dropping the oldest** when full; `capacity` required |
| `sync_set` | grouped alignment: `sets` partitions the ports; aligned within a group, independent across groups |
| `batch` | accumulate `capacity` packets and deliver them in one activation (single input port in this version) |

`fixed_size` is **deliberately lossy** and does not block upstream, which is what makes it the
memory-bounding companion to unbounded internal edges. It is never silent: the first drop logs a
warning and `lmflow_graph_dropped_count` counts them.

### Other node behaviour

- **`on_error`** — `abort` (default) fails the whole graph on any kernel failure. `skip` discards
  just that packet, advances the downstream timestamp bound, counts it in `errors` and logs a
  warning with exponential backoff. Use it for long-running pipelines where one bad frame should not
  be fatal. It applies to per-packet failures only: an `Open` or `Close` failure is a one-off
  lifecycle problem and should — and does — fail `start()`.
- **`rate`** — source pacing in Hz. The engine guarantees at least `1/rate` seconds between
  `process` calls, and throttles before entering the kernel while holding no engine lock. Sources
  only; setting it on a non-source is a build-time error.
- **`back_edges`** — names an input port as a latest-value feedback register: capacity 1, keeps the
  newest value, consumed once, and excluded from readiness, termination and timestamp alignment.
  This is what allows a topology to contain a cycle; a cycle not broken by a back edge is still
  rejected at build time.
- **`subgraphs` / `type` / `include`** — define a reusable subgraph once and instantiate it with
  `type:`; it is inlined at graph-build time, so the runtime never sees a subgraph.

Side packets carry constants that `options` cannot express — a loaded model, for instance. Set them
before `start`:

```c
LMFlowStatus lmflow_graph_set_side_packet(LMFlowGraph*, const char* name, LMFlowPacket pkt);
```

## Observability

The engine cannot interrupt a stuck kernel, so it makes stuckness *visible*. If a kernel blocks on a
socket or a lock it occupies an executor thread; a few of those drain the pool and the graph goes
quiet. `wait_done_timeout` would only tell you "timed out" — not which node.

```c
LMFlowNodeStats st = { .struct_size = sizeof(st) };
if (lmflow_graph_node_stats(graph, i, &st) && st.running && st.running_for_us > 5000000) {
  fprintf(stderr, "node %s has been running for %lld us\n", st.node_name,
          (long long)st.running_for_us);
}
```

| Field | Meaning |
|---|---|
| `struct_size` | **in**: you must set `sizeof(LMFlowNodeStats)` |
| `node_name`, `kernel_name` | lifetime follows the graph |
| `running`, `running_for_us` | currently inside a callback, and for how long |
| `processed`, `errors` | cumulative activation and failure counts |
| `total_process_us`, `max_process_us` | cumulative and worst-case callback duration |
| `packets_in`, `packets_out` | packets taken from inputs, packets dispatched downstream |
| `peak_queue_depth` | high-water mark observed when enqueuing downstream |
| `queued` | current backlog across all of this node's input edges |

`struct_size` is an **overflow guard**, not a compatibility shim: if it is smaller than the engine's
own `sizeof`, the call fails cleanly rather than writing past the end of your buffer. When statistics
are added, an old host recompiles and gets a clear error instead of memory corruption.

Lossless internal backpressure is observable per node input port:

```c
for (size_t p = 0; p < lmflow_graph_node_num_input_ports(graph, i); ++p) {
  LMFlowInputQueueStats q = { .struct_size = sizeof(q) };
  if (lmflow_graph_input_queue_stats(graph, i, p, &q) && q.blocked) {
    fprintf(stderr, "%s.%s blocked for %llu us (producer=%s)\n",
            q.node_name, q.port_name,
            (unsigned long long)q.blocked_for_us, q.producer_name);
  }
}
```

`queued_packets/bytes` are already in the consumer queue; `reserved_packets` are atomically
reserved by an upstream flush whose staging has not yet been dispatched. `peak_queued_*` are queue
high-water marks. `block_events` counts transitions into backpressure, while `total_blocked_us`
includes both completed waits and the currently active wait. `packet_capacity` is `0` when
unbounded; byte values are diagnostic only.

The engine also reports internal backpressure through the configured log callback. Events 1, 2,
4, 8, ... emit a warning with producer, consumer port, effective capacity, queued, reserved, and
incoming packet counts. When that warned event clears, an info message reports the blocked
duration. A terminal stall error includes the same queue-level context.

Rust hosts can additionally call `Input::backpressure_stats()` for global-watermark waits and
`Poller::backpressure_stats()` for bounded-Poller waits and drops. Both snapshots expose active
waiters, block events, current/cumulative blocked time, and the relevant capacity/queue context.
`graph.dump()` includes matching `watermark` and `poller` diagnostic lines. The same information is
reported through exponentially rate-limited WARN messages and matching recovery INFO messages.

No additional host-side query API is required for visualization. Use
`lmflow_graph_to_dot_view(g, LMFLOW_DOT_COMPACT)` for a lower-noise live view, or
`LMFLOW_DOT_DIAGNOSTICS` for full queue and backpressure detail. Diagnostics includes the global
watermark once in the graph title. Graph input
ports show only their own wait count/duration, avoiding the impression that the graph-wide limit is
per-port. Consumer edges show queue capacity/occupancy/reservations and block history; each Poller
is a cylinder with its policy, capacity, occupancy, drops, and block history. Active stalls are red
and thick; recovered stalls or drops remain amber so transient incidents stay visible. Multi-input
nodes contain a compact port summary. For sync-style policies, an empty open port that is preventing
an already-full sibling from draining is highlighted yellow as `WAITING`; immediate-policy inputs
are not inferred this way. Healthy edges retain only their port name. Durations automatically use
microseconds, milliseconds, or seconds; the title shows elapsed time since the current `start`.
A diagnostics legend explains red/yellow/amber states and dashed Poller subscriptions. SVG output
also carries hover tooltips with the full node, input-edge, and Poller snapshot. Every duration in
one statistics-enabled export is calculated from the same snapshot timestamp.

Both compact and diagnostics views label each node as `CREATED`, `IDLE`, `RUNNING`, `CLOSED`, or
`ERROR`. The border carries that state (green and thick for running, red and thick for error), while
the fill remains the latency heat map. Compact mode omits the per-port table, detailed edge
backpressure labels, Poller cylinders, and diagnostics legend.

The graph title also summarizes current hotspots: running nodes, error nodes, blocked producers,
likely waiting inputs, and dropped packets. Compact mode suppresses zero-valued packet/queue detail
for inactive nodes so large graphs remain scannable. Long visible labels are truncated with an
ellipsis, while SVG tooltips keep the complete node, kernel, namespace, executor, edge, and port
names. Nodes carry executor groups and stable state ordering; Graphviz rank/node spacing is tuned
without `concentrate`, so separate multi-port edges are never merged.

Statistics-enabled views also show a sampling window. The first export covers the current run from
`start`; later exports use the interval since the previous export of the same view. Node labels keep
cumulative totals while adding interval packet deltas, rates, input/output deltas, and interval
latency. Diagnostic edges and Pollers add interval backpressure duration/event deltas and drops.
Port annotations name the likely bottleneck directly: a full consumer queue, a missing aligned
input, a slowly draining downstream consumer, the global packet limit, or a slow/dropping Poller.
Compact and diagnostics views keep independent baselines, and `reset` clears both.

Diagnostics also ranks the five most actionable nodes and input ports as `HOT #1` through
`HOT #5`. Active queue-full or aligned-input stalls seed an upstream traversal: the affected
producer chain is drawn in purple as `PRESSURE PATH`, while the direct cause keeps its stronger
red or yellow styling. This separates the place where pressure is observed from the upstream
operators whose output is currently unable to advance. See
`lmflow/examples/python/live_diagnostics/` for a host loop that writes periodic DOT snapshots and
renders SVG files when Graphviz is available.

```c
int64_t      lmflow_graph_counter_value(LMFlowGraph*, const char* name);
size_t       lmflow_graph_counter_count(LMFlowGraph*);
const char*  lmflow_graph_counter_name(LMFlowGraph*, size_t idx);
const char*  lmflow_graph_dump(LMFlowGraph*);
const char*  lmflow_graph_to_dot_view(LMFlowGraph*, LMFlowDotView view);
size_t       lmflow_graph_queue_depth(LMFlowGraph*, const char* port);
uint64_t     lmflow_graph_dropped_count(LMFlowGraph*, const char* port);
size_t       lmflow_graph_total_queued(LMFlowGraph*);
uint64_t     lmflow_graph_total_queued_bytes(LMFlowGraph*);
LMFlowGraphState lmflow_graph_state(LMFlowGraph*);
```

`lmflow_graph_to_dot_view(g, LMFLOW_DOT_DIAGNOSTICS)` emits Graphviz DOT with a per-node latency
heat map — pipe it through `dot -Tsvg` to see where time goes. Nodes also show aggregate
queued/peak bytes, block event count, total blocked time, and the number of currently blocked input
ports. Counters set with
`Context::CounterAdd` are aggregated per graph and are far easier to assert on in a test than log
output.

`watchdog_ms` logs a warning when a single callback exceeds the given duration. `stats_timing`
(default on) controls whether each callback is timed; turning it off saves two clock reads per
`process`, at the cost of zeroing the timing fields and flattening the heat map. Because the watchdog
depends on those timings, setting `watchdog_ms > 0` forces timing back on and logs an INFO line
explaining why — a silently disabled watchdog is not an acceptable outcome.

## Logging

```c
void lmflow_set_log_callback(void (*cb)(void* user, LMFlowLogLevel level, const char* msg),
                             void* user);
```

The engine is silent unless you install a sink — a library should not commandeer the host's stdout.
The callback may fire on any worker thread, so it must be thread-safe. The engine holds **no internal
lock** while calling it, which means locking or acquiring the GIL inside is safe and cannot form a
lock-order cycle with the engine. Do not call `lmflow_graph_*` from inside it.

On a platform with a system logger, one line is enough:

```cpp
#include "lmflow/flow_platform_log.hpp"
lmflow::InstallPlatformLogSink();   // → logcat / os_log / HiLog
```

Link requirements: Android needs `-llog`, OpenHarmony needs `libhilog_ndk.z.so`, and on Apple
platforms `os_log` is already in libSystem.

## Threading and lifetime rules

The rules that actually bite, in one place:

1. **The engine holds no internal lock while a kernel runs.** Blocking, locking or taking the GIL
   inside `Open` / `Process` / `Close` is safe.
2. **`LMFlowContext*` and `LMFlowContract*` live only for their callback.** Storing either, or a
   packet borrowed from one, is a dangling reference.
3. **Observer and log callbacks run on the dispatching thread** — a pool thread if the producing node
   has an executor. Make them thread-safe, and do not re-enter `lmflow_graph_*` from them.
4. **Nodes without an `executor` run on the default executor**, a thread pool sized to the CPU
   count. So the default *is* concurrent and execution order is not deterministic. Declare
   `- { name: "", type: "DelegatingExecutor" }` to hand the default back to the host thread and get
   deterministic, zero-concurrency execution (pumped during blocking host calls).
5. **A source node (no input ports) cannot run on a delegating executor.** Its `process` typically
   blocks waiting for the next frame, which would monopolise the host thread and stall the graph.
   Nor may a pool carry as many source nodes as it has threads — that starves everything else on it.
6. **Graph input timestamps must strictly increase**, and `LMFLOW_TS_UNSET` is not allowed there.
   Sentinels: `UNSET` < `UNSTARTED` < `PRE_STREAM` < `MIN` … `MAX` < `POST_STREAM` <
   `ONE_OVER_POST_STREAM` < `DONE`.

## OpenCV interop

`flow_cv.hpp` is optional, outside the ABI, and imposes no OpenCV dependency on the engine — the
core and both main headers build with no image library present.

```cpp
namespace lmflow {
int      CvDepth(int32_t dtype);
int32_t  DtypeFromCv(int cv_depth);
cv::Mat  CvWrap(const LMFlowBuffer& b);                 // zero-copy, no ownership
const cv::Mat CvView(const Packet& pkt);                // read-only zero-copy view
LMFlowStatus  CvMutable(Packet& pkt, cv::Mat* out);     // writable; copies only if shared
Packet   NewMatPacket(int rows, int cols, int channels, int32_t dtype, cv::Mat* out);
Packet   PacketFromMat(const cv::Mat& m);               // copies m into a new packet
}
```

`ndim == 2` is treated as single-channel `[H, W]`; `ndim == 3` as `[H, W, C]`. A returned `cv::Mat`
is valid only while the underlying `Packet` lives.

In-place processing follows the take-then-mutate rule:

```cpp
lmflow::Status Process(lmflow::Context& cc) override {
  lmflow::Packet p = cc.TakeInput(0);          // sole reference → mutation stays copy-free
  cv::Mat m;
  LMFLOW_RET_CHECK(cc, lmflow::CvMutable(p, &m) == LMFLOW_OK);
  cv::GaussianBlur(m, m, {5, 5}, 0);
  cc.Emit(0, std::move(p));
  return lmflow::Status::Ok();
}
```

Producing a new image is best done by letting the engine allocate:

```cpp
cv::Mat out;
lmflow::Packet p = lmflow::NewMatPacket(h, w, 3, LMFLOW_DTYPE_U8, &out);
// ... fill `out` ...
cc.Emit(0, std::move(p).At(cc.InputTimestamp()));
```

## The bundled C++ kernels

Available after `lmflow_register_builtin_kernels()`. **The registered names all carry the `Kernel`
suffix** — that is what YAML must say.

`PassThroughKernel`, `ScaleKernel`, `SumKernel`, `SplitKernel`, `ZipKernel`, `FilterKernel`,
`StringifyKernel`, `SinkKernel`, `InvertKernel`, `NormalizeKernel`, `MuxKernel`,
`RangeSourceKernel`, `FeedbackAddKernel`, `BatchSumKernel`, `CastKernel`, `AffineKernel`,
`ClampKernel`, `ReduceKernel`.

They exist to demonstrate engine semantics — reading options, emitting on close, timestamp
alignment, advancing bounds, `SourceDone` — and double as worked examples; see
[`lmflow/cpp/kernels/`](https://github.com/laomou/lm-flow/tree/main/lmflow/cpp/kernels). They are
present in the released SDK's `liblmflow.a`, but **not** in the crate published to crates.io, since
their sources live outside the crate directory.

The engine's own two Rust kernels, `PassThrough` and `Sink`, are always available and need no
registration call. Note the missing suffix: the names are deliberately distinct from the C++ ones,
because the registry is keyed by name and a duplicate registration is an error.

## Mobile embedding

Link the **static** library. Working integrations:

- [Android (JNI)](https://github.com/laomou/lm-flow/tree/main/lmflow/examples/android/hello_world)
- [iOS (Swift)](https://github.com/laomou/lm-flow/tree/main/lmflow/examples/ios/hello_world)
- [HarmonyOS (NAPI)](https://github.com/laomou/lm-flow/tree/main/lmflow/examples/harmonyos/hello_world)

Call `lmflow::InstallPlatformLogSink()` early so engine diagnostics land in the platform log.

## Where next

- [Rust API reference](../rust/) — for a Rust host or Rust kernels
- [Python API reference](../python/) — for the `lmflow` Python package
- [Design document](../design/) — scheduling model, timestamp and termination semantics, lock
  ordering rules, and the decision log (Chinese)
- [Runnable C++ examples](https://github.com/laomou/lm-flow/tree/main/lmflow/examples/cpp) —
  `hello_world` (external host against `flow.h`) and `custom_type` (a custom C++ payload)
