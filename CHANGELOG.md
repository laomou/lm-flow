# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Three artifacts ship from one version number: the `lmflow` crate on crates.io, the
`lm-lmflow` wheel on PyPI (imported as `lmflow`), and the per-platform native SDK attached
to each GitHub Release.

## [Unreleased]

### Added

- **Vulkan adapter: GPU→CPU staging read-back (`lmflow::vk::Download`).** Device-only memory — a
  device with no `DEVICE_LOCAL|HOST_VISIBLE` type, i.e. the classic discrete-GPU case — now reads
  back by copying the device buffer through a host-visible staging buffer, mirroring the existing
  upload staging path in reverse (device → staging → host). The full GPU→CPU round trip therefore
  works on such devices. `Context::SubmitLocked` gained an optional wait-stage argument so the
  read-back copy waits on the producer at the `TRANSFER` stage; existing compute submits are
  unchanged. The staging upload/read-back code paths are now exercised on unified-memory devices
  (including CI's lavapipe) via `LMFLOW_VK_FORCE_STAGING=1`, where a second ctest
  (`lmflow_vulkan_resize_test_staging`) re-runs the same resize cases with staging forced and
  checks them against the same CPU reference as the direct-map run, so both paths are validated
  against one oracle.
  The hardware precondition — a device that truly lacks a host-visible memory type — remains
  unverified and still requires real discrete-GPU hardware.

### Fixed

- **Vulkan buffer pool: reuse was gated on the memory allocation size, not the buffer size.**
  `Image::Allocate` recorded `capacity = requirements.size` while creating the `VkBuffer` with
  `size = bytes`. The Vulkan spec permits `requirements.size > info.size`, so on a driver that pads,
  a request could match — and reuse — a `VkBuffer` genuinely too small for it: `vkCmdCopyBuffer`
  would copy out of bounds and the `VK_WHOLE_SIZE` descriptor would resolve to a shorter range than
  the host assumed. `capacity` is now the buffer's creation size. Note CI cannot catch this class of
  bug: lavapipe reports zero padding for every size measured, so the reuse mismatch never occurs
  there. The staging pool was already correct; the two now agree.
- **OpenCL buffer pool: reuse ignored every `cl_mem_flags` bit except `CL_MEM_ALLOC_HOST_PTR`.**
  `Image::Allocate` takes caller-supplied flags (defaulting to `CL_MEM_READ_WRITE`), so a host that
  allocated with `CL_MEM_READ_ONLY` — or any `CL_MEM_HOST_*` restriction — could have that buffer
  recycled and handed to a default read-write request as a kernel *output*, which is undefined
  behaviour per the OpenCL spec and entirely silent. Slots now record the effective flags and reuse
  requires them to be equal.
- **OpenCL buffer pool: recorded capacity ratcheted downwards.** `Image::Reset` recorded
  `capacity = byte_size()` (the logical size) rather than the size the buffer was created with, so
  a large buffer that had been reused by a smaller request came back to the pool understating its
  capacity. Large requests then stopped matching it while it kept occupying a slot and its real
  device memory. `Image` now carries the creation size through to the pool.
- **Both adapters: a full pool discarded the buffer that had just been returned.** All three pools
  destroyed the incoming buffer once full, keeping the eight older ones. Under a workload whose
  sizes grow, the pool filled with buffers too small to ever match again while every new larger
  buffer was destroyed on return — the pool never hit and still held the memory. A full pool now
  evicts its *smallest* slot instead (which may be the incoming one, when that is the smallest).
- **Both adapters: pools were bounded by slot count only.** Eight 24 MB frames is 192 MB retained
  for the process lifetime — staging is host-visible memory, the `Image` pool is device memory, and
  neither is trimmed before `Context` teardown. Both pools now also carry a byte ceiling.

### Changed

- **Vulkan adapter: the staging and `Image` pools share one implementation.** They were near-identical
  copies, which is how the eviction bug above came to exist in both; `Retired` likewise carried two
  parallel `recycle_*` booleans. There is now a single `BufferSlot`/`BufferPool` with one
  `TryAcquireLocked`/`RecycleLocked` pair, and `Retired` carries one `Reclaim` tag.
- **Both adapters: pool hit/miss counters, and tests that assert on them.** The only benefit pooling
  offers is "steady-state sizes stop asking the driver for memory", and nothing covered it — the
  numeric tests do not care how many allocations happened, so making the pool never hit would have
  left every test green. New `lmflow_vulkan_pool_test` / `lmflow_opencl_pool_test` assert zero new
  allocations across 20 same-size rounds, that a full pool still serves the largest size (this fails
  against the old eviction policy), that OpenCL never crosses flag boundaries (this fails against the
  old flag matching), and that neither ceiling is exceeded.


- **Vulkan adapter: staging buffers are pooled and reused instead of allocated per transfer.**
  On the staging upload/read-back paths (the discrete-GPU case, and any run with
  `LMFLOW_VK_FORCE_STAGING=1`), each `Upload`/`Download` previously created a host-visible buffer
  with its own `vkAllocateMemory` and destroyed it after the copy. Staging buffers are now drawn
  from a best-fit pool on the `Context` (usage `TRANSFER_SRC|TRANSFER_DST`, so one pool serves both
  directions) and returned to it once the transfer's timeline value completes — the *same* deferred
  reclamation that already made destruction safe now makes recycling safe, so there is no host wait
  and no new use-after-free surface. A steady-state fixed-size resize allocates staging once and
  then reuses it every frame (measured on lavapipe: 10 staging transfers → 2 allocations + 8
  reuses), keeping `vkAllocateMemory` clear of `maxMemoryAllocationCount`. Output is unchanged —
  `lmflow_vulkan_resize_test_staging` re-runs the same cases with staging forced and checks them
  against the same in-test CPU reference (1e-3 tolerance) as the direct-map run.
- **Vulkan & OpenCL adapters: device buffers (`Image`) are pooled and reused instead of allocated
  per upload/dispatch.** Every `Upload` and every dispatch output previously created a device
  buffer with its own allocation (`vkCreateBuffer` + `vkAllocateMemory` on Vulkan;
  `clCreateBuffer` on OpenCL) and destroyed it when the last reference dropped. Compute buffers
  are now drawn from a best-fit pool on the `Context` and returned to it once the GPU is done with
  them, so a steady-state pipeline allocates once and then reuses every frame (measured on
  lavapipe: the 5-case resize test does 5 allocations + 5 reuses; the bench's tight loop runs
  clean). Pooling changes nothing semantically — memory type / host-mapped flags are matched or
  re-derived, so zero-copy download and forced-staging paths behave identically.
  - **Vulkan** recycles through the *same* timeline-based deferred reclamation that already made
    destruction safe: a buffer goes back to the pool only when `ReclaimUpTo` sees its timeline
    value completed, so there is no host wait and no new use-after-free surface. The pool splits
    device-local vs host-visible buffers so unified and discrete paths never mix.
  - **OpenCL** relies on the single in-order queue: a recycled `cl_mem` is reused by a new
    `Image`, and the new producer's enqueue is sequenced after every old consumer command, so the
    "return must follow the sync point" requirement holds by queue ordering — no host wait. (This
    was noted as deferred in earlier code; the in-order queue makes it free.) Pooled buffers are
    split by the `CL_MEM_ALLOC_HOST_PTR` flag so zero-copy download buffers never mix with plain
    ones.
  - Both pools are bounded (8 slots) so size-growing workloads can't accumulate stale buffers;
    each adapter's `Context` destructor destroys whatever remains.
- **Vulkan adapter: `VkDownload` no longer fails at `Open` on device-only memory.** With staging
  read-back implemented — and guaranteed feasible, since the Vulkan spec requires at least one
  `HOST_VISIBLE|HOST_COHERENT` memory type — the previous "staging read-back path is not
  implemented" rejection is removed.

### Fixed

- **Output poller could silently drop tail packets at end-of-stream under load.** `Poller::next`
  (and `try_next_result`) treated an edge's `closed` flag as "the queue is empty" and returned `None`
  without a final drain. But `closed` only latches "no more packets will be *enqueued*": a producer
  can enqueue the last packet(s) and close the edge in the window between the poller's empty `pop`
  and its `closed` check, so a consumer preempted in that window (common on a busy/few-core machine)
  abandoned packets that were already queued. The closed branch now does a final `pop`, mirroring
  the existing idle branch. This is the root cause of the rare
  `peak_queue_depth_is_a_high_water_mark` flake observed on the macOS CI runner; a contended
  regression test (`blocking_next_drains_tail_when_edge_closes_mid_drain`) drops packets without
  the fix and passes with it.
- **`lmflow_poller_try_next_status` could report `CLOSED` with packets still queued.** The same race
  one level up, at the C ABI: on `Ok(None)` the entrypoint decided `CLOSED` vs `WOULD_BLOCK` from a
  *second, later* `is_closed()` load, so a producer that enqueued and closed in between made it
  report end-of-stream over a non-empty queue — and a C consumer treating `CLOSED` as "stop draining"
  lost those packets. It now requires `is_closed() && is_empty()`, returning `WOULD_BLOCK` while the
  queue still holds packets. `lmflow_poller_next_status` / `next_timeout` were never affected (their
  `Ok(None)` already includes the final drain), nor was
  `lmflow_kernel_runner_try_output` (it maps `Ok(None)` to `WOULD_BLOCK` unconditionally).

## [0.3.1] — 2026-08-18

### Changed

- Bump the crate, Python package, native SDK, and bundled examples to `0.3.1`.

## [0.3.0] — 2026-08-07

### Added

- Output Pollers and observers can now opt into timestamp-bound events through the existing
  `lmflow_graph_add_poller_ex` / `lmflow_graph_observe_ex` APIs. Bounds arrive as monotonic empty
  packets and terminate with `LMFLOW_TS_DONE`; Rust and Python expose matching opt-in helpers.

- Optional OpenCV interop now provides `lmflow::AdoptMat` for zero-copy ownership of normal
  `cv::Mat` allocations and non-contiguous ROIs, while preserving copy-on-write isolation.

- Python `Packet.from_numpy()` and `send(ndarray)` now adopt NumPy storage without copying. The
  array remains alive and read-only while retained by the graph; final release safely reacquires
  the GIL, restores its original writeability, and copy-on-write protects Python-owned input.

- `lmflow_packet_adopt_buffer` and `lmflow::Packet::AdoptBuffer` provide zero-copy ownership
  transfer for validated external CPU buffers. The last packet reference invokes a caller-provided
  release callback exactly once; read-only or shared adopted buffers copy only when made mutable.

### Fixed

- **External buffer descriptors are validated before memory access.**
  `lmflow_packet_from_buffer` now rejects non-CPU devices, unknown flags, non-zero reserved or
  unused fields, invalid dimensions/dtypes/strides, null data for non-empty views, and overflowing
  allocation sizes or pointer offsets before dereferencing the caller's data. Valid
  negative-stride, broadcast, and non-contiguous CPU views remain supported.
- **Thread-pool CoW no longer retains producer references across dispatch.**
  Invocation inputs are released as soon as the kernel returns, and downstream workers are
  woken only after producer staging references are cleared. Linear pipelines therefore keep
  zero-copy mutation on thread pools, while fan-out still copies to preserve branch isolation.

### Changed

- **BREAKING — custom types now require one canonical descriptor.** The name-only
  `lmflow_register_type_name` C API is removed. Custom `type_id` values must equal
  `lmflow_type_id(stable_name)`, and registration always supplies the stable name, size, and
  alignment through `lmflow_register_type_descriptor`. C++, Rust, C, and Python now use the same
  stable-name hash entry point. Removing the old C ABI symbol bumps `LMFLOW_ABI_VERSION` from 3 to
  4.
- **BREAKING — kernel registration is now link-driven.** Bundled C++ kernels self-register when
  `lmflow::lmflow` / `lmflow::kernels` is linked. The manual
  `lmflow_register_builtin_kernels` entry point and the name-only
  `lmflow_registered_kernel_count` / `lmflow_registered_kernel_name` enumeration APIs are removed;
  Python likewise no longer exposes `register_builtin_kernels()` or `registered_kernels()`. The
  pure Rust engine also no longer installs `PassThrough` / `Sink` while building a graph; Rust
  hosts register every kernel explicitly. These removed C ABI symbols bump
  `LMFLOW_ABI_VERSION` from 2 to 3.
- **Graph runtime module layout.** The former monolithic graph implementation is split into
  dedicated Poller, node/readiness, backpressure, runtime scheduling, and lifecycle files without
  changing the public API or execution behavior.
- **BREAKING — nodes without an `executor` now run on a real default thread pool, not the host
  thread.** The engine creates a default executor named `default`, sized to
  `available_parallelism()`, and every node that names no executor is bound to it. Previously such
  nodes ran on the host thread and only advanced while the host sat inside `wait_done` /
  `wait_until_idle` / `poller.next` / a blocking `send` — a graph that only ever called `send` never
  progressed at all. That surprise is gone; the cost is that **default execution is now concurrent
  and its order is no longer deterministic**, and Python kernels now contend for the GIL by default.
- **BREAKING — `Graph::executor_names()` now includes the default executor as its first entry.**
  A graph declaring one pool `cpu` reports `["default", "cpu"]` rather than `["cpu"]`.
- **BREAKING — the DOT string `@main` no longer exists.** Node placement is always rendered as
  `@<executor name>`, so host-thread nodes now show `@default` (or whatever the delegating executor
  is called). Delegating executors keep the white fill and are listed in the executor legend as
  `host thread (delegating)`; the standalone `legend_main` box is gone.
- **`max_in_flight > 1` is now validated against the resolved executor's thread count** rather than
  against "did you write an `executor` field". Since the default pool is multi-threaded, a node on
  the default executor may now set `max_in_flight > 1` (ADR #29). Single-threaded pools and
  delegating executors still reject it.
- **Source nodes are rejected only on delegating executors**, not on "no executor" — the default
  pool is a valid home for a source. Multiple cooperative sources may share even a single-thread
  pool; a source that blocks inside its own `process` still occupies that worker.

### Added

- **`DelegatingExecutor` — host-thread execution is now an executor type**, selected with
  `type: "DelegatingExecutor"`. It owns no threads and hands ready nodes back to the host thread,
  restoring the old default's guarantees: zero concurrency, deterministic order, straightforward
  debugging, and Python kernels free of GIL contention. Declare one and point nodes at it:

  ```yaml
  executors:
    - { name: "host", type: "DelegatingExecutor" }
  nodes:
    - { name: draw, kernel: Overlay, executor: "host" }
  ```

  `num_threads` / `affinity` / `priority` on a delegating executor are rejected rather than silently
  ignored.
- **`default` is now a reserved executor name.** Everything in `executors` is the host's own
  executor and must be named; the default executor is engine-owned and not configurable. Declaring
  `name: "default"`, or an entry with an empty name, is an error. To control threads / affinity /
  priority, declare your own pool and point the nodes at it.
- **`Graph::pump_step()`** is documented as the way for a host that owns its own event loop to
  advance delegating-executor nodes without blocking, and is now exposed consistently through the
  Rust API, C ABI (`lmflow_graph_pump_step`) and Python binding.
- **Event-loop wakeups for delegated execution.** Rust `Graph::set_wakeup_callback` and C
  `lmflow_graph_set_wakeup_callback` provide a coalesced edge-triggered notification when the host
  should drain `pump_step()`. Qt/libuv hosts can post it into their own loop; Python offers
  `await graph.run_async()` backed by `asyncio.call_soon_threadsafe`, without polling.
- **Delegated execution is serialized and fair per graph.** Concurrent host callers cannot execute
  delegated kernels simultaneously, and multiple `DelegatingExecutor` queues are pumped
  round-robin instead of giving permanent priority to the first one.
- **Cooperative source scheduling.** Source kernels can call Rust `source_yield(Duration)`, C/C++
  `lmflow_ctx_source_yield` / `Context::SourceYield(delay_ms)`, or Python
  `cc.source_yield(delay_seconds)` to release their worker and request a later invocation. YAML
  `rate: N` now uses the same delayed executor queue instead of sleeping on a worker thread.
- **Executor runtime state in DOT diagnostics.** Executor legend boxes now show current queued and
  running tasks, thread capacity, peak queued tasks, and completed tasks. These counters are
  embedded in compact/diagnostics visualization rather than exposed as another public stats API.
- **Python `lmflow.KernelError`.** An execution failure now raises a dedicated exception instead of
  a bare `RuntimeError`, so hosts can tell "a kernel failed" apart from a cancellation or a
  bad-state error. It derives from `RuntimeError`, so existing `except RuntimeError` code keeps
  working. Note the engine reports graph stalls with the same status code, so an unsatisfiable
  graph surfaces here too — the message distinguishes them (`kernel failed: ...` vs `wait_done: ...`).

### Fixed

- **A graph whose every node named an executor no longer logs a bogus
  ``executor `` is defined but not used`` warning.** The implicitly created default executor is
  exempt from the unused-pool check.

### Added

- **Layered Graphviz views and live node states.** Rust, C, and Python hosts can select topology,
  compact, or full diagnostics output. Compact mode keeps node state and core throughput/latency
  statistics while hiding per-port and Poller detail. Statistics-enabled views show
  `CREATED`/`IDLE`/`RUNNING`/`CLOSED`/`ERROR` with border colors, leaving node fill available for
  the latency heat map.
- **Native Windows/MSVC CI coverage.** Windows now builds and runs the Rust engine with the
  bundled C++ kernels, compiles the public C/C++ headers and header-only tests with MSVC, validates
  Debug and Release Visual Studio CMake builds, installs and consumes the native SDK through
  a minimal `find_package` ABI smoke, and builds/runs the Python extension. CMake now selects
  `lmflow.lib`, propagates
  Rust's required Windows system libraries, and keeps the MSVC runtime consistent with rustc.
- **Backpressure-aware graph visualization.** Statistics-enabled Graphviz output now shows the
  global packet watermark, per-input queue capacity/occupancy/reservations, and Poller
  capacity/occupancy/drop state. Multi-port nodes include a compact port table; active stalls are
  red, likely missing aligned inputs are yellow, and historical stalls or drops are amber. Healthy
  edges stay compact. Durations use adaptive units, a diagnostics legend explains the styling,
  SVG tooltips retain detailed snapshots, and the title includes the current run's elapsed time.
- **Ranked hotspot and pressure-path visualization.** Diagnostics ranks the five most actionable
  nodes and input ports, then traces active queue/alignment stalls backward through their producer
  chain. Direct causes retain red/yellow styling while upstream propagation is purple. A new Python
  example periodically exports DOT and optionally renders live SVG snapshots with Graphviz.
- **Global-watermark and Poller backpressure diagnostics.** Rust input and Poller handles expose
  active waiters, event counts, and blocked durations. Exponentially rate-limited WARN/INFO logs
  and graph dumps include port, policy, capacity, occupancy, timeout, and drop context; reset clears
  the new runtime statistics.
- **Actionable internal backpressure diagnostics.** Exponentially rate-limited WARN messages now
  identify the producer, consumer port, effective capacity, queue occupancy, reservations, and
  incoming batch. Matching INFO messages report recovery duration, while terminal stall errors
  list the exact blocked queue relationships instead of producer names alone.
- **Per-input queue backpressure observability.** Rust and C hosts can inspect current and peak
  packet/byte occupancy, pending reservations, active producer blocking, block event counts, and
  cumulative blocked time. DOT and graph dumps include aggregate backpressure diagnostics, with
  reset, cancellation, `max_in_flight`, and long-running diamond coverage.
- **Per-port packet capacities for internal inputs.** The unified `input_queues` object provides
  node defaults plus per-port packet overrides. Queue byte occupancy remains observable for
  diagnostics, but capacity enforcement is intentionally packet-only.
- **Cooperative lossless backpressure for internal edges.** A node can set `input_queues.packets`
  to bound each forward input queue without blocking executor threads.
  Producers retain completed staging and yield; dequeue resumes the pending flush. Source nodes,
  diamond fanout/join graphs, close output, cancellation, and impossible alignment stalls are
  covered by dedicated tests. The option is mutually exclusive with lossy `fixed_size`.
- **Bounded Poller queues and complete output accounting.** Poller-retained packets now count
  toward the global packet watermark and packet/byte diagnostic counters. New bounded Pollers support `block`, `drop_oldest`,
  `drop_newest`, and capacity-1 `latest`; lossy policies expose drop counts and warnings.
  Releasing a Poller unregisters it, clears its accounted queue, and wakes blocked producers.

- **Two-level port type checking.** When a producer output and consumer input both declare
  concrete, different types, graph construction now fails immediately with the edge, nodes,
  ports and type names in the diagnostic. An `any` endpoint remains dynamic and is checked
  packet-by-packet at runtime. Kernel emissions are also validated against the kernel's own
  output contract before dispatch, including packets emitted from `Close` or directly to a
  graph output.
- **`InteropType` for Rust-defined cross-language payloads.** An unsafe implementation
  centralizes the ABI-layout and stable-type-name promise; `Packet::from_interop` derives and
  registers the type id from that name.

### Changed

- **GitHub Actions Node 24 migration.** Official checkout, Python setup, artifact, and Pages actions
  now use their Node 24-based major versions. Rust toolchain component lists are quoted so YAML does
  not reinterpret `clippy` or `miri` as unexpected action inputs.
- **Large-graph Graphviz readability.** Statistics-enabled titles summarize running, error,
  blocked, waiting, and dropped hotspots. Compact view omits zero-valued throughput and queue
  details from inactive nodes. Long node, kernel, cluster, executor, and port labels are truncated
  while SVG tooltips retain full names. Stable executor/state ordering, executor groups, and tuned
  rank spacing reduce crossings without merging distinct multi-port edges.
- **Interval Graphviz diagnostics and bottleneck reasons.** Statistics views retain cumulative
  totals while showing deltas and throughput since the previous export of the same view (or since
  `start` for the first export). Edges and Pollers include interval backpressure/drop changes and
  identify likely causes such as a full consumer queue, missing aligned input, slow downstream
  drain, global packet watermark, or slow/dropping subscriber. Reset clears the private baselines.
- **Portable Python binding type identities.** Binding-only wrapper classes now live outside the
  public C++ `lmflow` namespace, eliminating LTO ODR conflicts with `flow.hpp`. MSVC registration
  bindings use unambiguous lambdas, and the Windows wheel step stops immediately if installation
  fails instead of running tests against a missing package. The thin binding module also disables
  pybind11's automatic full LTO, avoiding serialized GCC LTRANS jobs while leaving the Rust engine
  optimized by its release profile.

### Fixed

- **Internal backpressure false-stall detection.** An idle snapshot with retained producer staging
  now triggers a full readiness rescan and flush retry before reporting a deadlock. This prevents
  bounded diamond pipelines from being rejected during a transient scheduling window while
  preserving errors for genuinely impossible input alignment.
- **C ABI version 2.** The boolean `lmflow_graph_to_dot` entry point is removed; callers must use
  `lmflow_graph_to_dot_view` with an explicit topology, compact, or diagnostics view.
- **Consistent and render-tested Graphviz snapshots.** One statistics-enabled DOT export now uses
  a single timestamp for the title, nodes, input queues, graph inputs, and Pollers. CI also renders
  representative plain and diagnostic DOT through Graphviz to catch invalid SVG output.
- **Rust API breaking change:** `Packet::new_interop` is now `unsafe`, because an arbitrary
  `T` plus arbitrary id cannot prove that foreign readers use the same ABI layout. It also
  rejects ids `0..=15`, which are reserved for engine-owned built-in layouts. Use
  `Packet::from_i64` / other built-in constructors, or implement `InteropType` and call
  `Packet::from_interop`.
- **Strict custom type descriptors.** Custom identities can now register the stable name,
  `size`, and `align` behind a `type_id`. Exact repeats are idempotent, while id/name/layout
  conflicts fail instead of silently passing a numeric hash comparison. C++ typed packets and
  contracts register automatically; Rust `InteropType` does the same. Registered fixed-size
  foreign payloads now contribute their shallow object size to byte-watermark accounting.

## [0.2.0] — 2026-08-04

### Added

- **Node-level error policy `on_error: abort | skip`.** `abort` (the default) keeps the
  historical behaviour of failing the whole graph on the first kernel error. `skip`
  discards only the offending packet, **advances the downstream timestamp bound**, counts
  it and logs a warning, then carries on — a long-running realtime pipeline should not die
  because one frame was bad. Advancing the bound is the essential part: without it, one bad
  frame escalates into a stalled graph. There are deliberately only two values; `skip`
  always counts and logs, so a separate `log` mode would add nothing. Applies to
  per-packet failures only — an `Open`/`Close` failure still fails `start()`.
- **Declarative source pacing `rate: N` (Hz).** The engine guarantees at least `1/rate`
  seconds between `process` calls on a source node, so the kernel needs no timing code of
  its own. Throttling happens before entering the kernel while holding no engine lock, and
  is measured from the actual release time to avoid drift. Source nodes only.
- **`Graph::reset()` — re-run while keeping opened kernel instances.** After termination, a
  graph can be reset and started again without re-running `Open`, so an expensive one-off
  such as loading a model is not repeated per session. Requires the graph to be terminated
  and idle. Exposed on all three surfaces: `Graph::reset()` (Rust), `lmflow_graph_reset()`
  (C ABI — the one added symbol this release), `Graph.reset()` (Python).
- **`LMFLOW_RET_CHECK(cc, cond)` / `LMFLOW_RET_CHECK_MSG(cc, cond, msg)`** in `flow.hpp`.
  Only an `int32_t` crosses the ABI, so the failure *text* travels a separate channel —
  meaning `return Status::Error()` hands the host a code with no reason. These macros bind
  "fail" and "say why" into one action, stamping in the stringified condition plus
  `__FILE__:__LINE__`. All 18 bundled kernels now use them. `KernelAdapter` also routes
  `std::exception::what()` into the context, so thrown exceptions no longer lose their text.
- **F16 (`binary16`) support in the tensor preprocessing kernels** — `Cast`, `Affine`,
  `Clamp`, `Reduce` now accept and produce `dtype: f16`. Conversion is a self-contained
  software implementation, deliberately not `_Float16` or F16C/NEON intrinsics: MSVC has no
  portable half type, intrinsics need per-architecture dispatch, and preprocessing is not
  the innermost inference loop. Because it depends on no compiler, the rounding behaviour is
  pinned by tests. Round-to-nearest, ties-to-even; `double → half` converts straight from
  the double bit pattern rather than via `float`, avoiding double rounding.
- **`batch` input policy now supports multiple input ports.** A batch is `capacity`
  *aligned tuples* — the `sync` alignment run `capacity` times — so per-port counts may
  differ when a port has no packet at a given aligned timestamp. It is deliberately *not*
  "each port counts to `capacity` independently", which would pair port 0's k-th packet with
  port 1's k-th packet even when they are different frames, silently. No API or ABI change
  was needed: `input_count(i)` / `input_at(i, k)` were already per-port.
- **Documentation site** at <https://laomou.github.io/lm-flow/> covering all three
  surfaces: Rust (rustdoc), C/C++ (hand-written guide), Python (pdoc), plus the design
  document. The Rust crate front page and the main public types are now documented in
  English, and `cargo test --doc` runs real examples for the first time.
- **Long-run memory soak test** for the global watermarks (`tests/soak.rs`, `#[ignore]` by
  default). Asserts that RSS growth is bounded by the *watermark* rather than by total
  throughput, so the bound is independent of run length.

### Fixed

- **Rare pool-drain false stall.** A claimed `max_in_flight` invocation is now counted before it
  leaves the node scheduler lock, closing a tiny window where `wait_done` could observe the pool as
  idle and incorrectly report unclosed nodes.
- **`cmake/engine.cmake` picked the wrong cargo profile under multi-config generators.**
  `CMAKE_BUILD_TYPE` is only meaningful for single-config generators; with Ninja
  Multi-Config or Visual Studio the configuration is chosen at build time via `--config`.
  The old configure-time check therefore fell back to `debug`, so
  `cmake --build --config Release` compiled the C++ side as Release while linking an
  **unoptimized debug engine**, with no warning. Single-config generators were unaffected.
- Several documentation defects that would actively mislead: `flow.h` listed the 18 bundled
  kernels **without** their `Kernel` name suffix (copying those names produces "kernel not
  registered"), claimed the `builtin-kernels` feature was on by default when it is off, and
  listed only three of the five input policies. `config.rs` claimed `max_in_flight > 1` was
  unimplemented. The Python `__all__` omitted four public symbols including
  `register_builtin_kernels()`, which is the first line of the README's Python quick start —
  so it was missing from the generated API docs.

### Changed

- `NormalizeTypeId` in `flow.hpp` is now `constexpr` (was `inline`), so the C++ side can
  assert the `type_id` hash at compile time. Source-compatible.
- The crate now declares `homepage` and `documentation` metadata.

### Notes

- `LMFLOW_ABI_VERSION` stays at **1**. This release only adds functions
  (`lmflow_graph_reset` and the type-descriptor queries/registration); no cross-boundary struct
  layout changed, which is the documented trigger for bumping it.
- Both F16 and multi-port `batch` *widen* what is accepted — configurations that previously
  failed at graph-build time now work. Nothing that used to work behaves differently.

## [0.1.1] — 2026-08-03

Release plumbing only, no engine changes: README install instructions, a CI guard that
keeps the crate publishable (`cargo publish --dry-run` — `cpp/` and `include/` live outside
the crate directory, so a path change could silently break publishing), and idempotent
re-runs of the release workflow.

## [0.1.0] — 2026-08-02

First public release. Rust engine (scheduler, executors, edge queues, topology, YAML)
behind a stable C ABI, with kernels written in Rust, C++ or Python through one
language-agnostic registry. Timestamp alignment and bound propagation, five input policies,
feedback edges, subgraphs, side packets, copy-on-write packets, per-node statistics with a
watchdog, DOT export, thread pools with CPU affinity and realtime priority, and
cross-compilation to Android / iOS / HarmonyOS.

Note: the `v0.1.0` git tag and its GitHub Release were removed after publication; the
crates.io and PyPI artifacts remain available.

[0.3.1]: https://github.com/laomou/lm-flow/releases/tag/v0.3.1
[0.3.0]: https://github.com/laomou/lm-flow/releases/tag/v0.3.0
[0.2.0]: https://github.com/laomou/lm-flow/releases/tag/v0.2.0
[0.1.1]: https://github.com/laomou/lm-flow/releases/tag/v0.1.1
[0.1.0]: https://pypi.org/project/lm-lmflow/0.1.0/
