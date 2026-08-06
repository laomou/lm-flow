# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Three artifacts ship from one version number: the `lmflow` crate on crates.io, the
`lm-lmflow` wheel on PyPI (imported as `lmflow`), and the per-platform native SDK attached
to each GitHub Release.

## [Unreleased]

### Added

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

### Known issues

- **CoW zero-copy on a linear pipeline is best-effort on thread pools, not guaranteed.** An upstream
  node's context input slot is only cleared at the start of its *next* call, so a downstream in-place
  write on another worker thread can still see a refcount ≥ 2 and silently copy. Measured over 600
  single-packet three-stage runs: 0 copies on a delegating executor, ~13% on a 4-thread pool. This
  predates the default change (any `executor:`-on-a-pool graph behaved this way), but the default
  path now lands on it. See `docs/design.md` §3.4.

## [0.3.0] — 2026-08-04

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

[0.3.0]: https://github.com/laomou/lm-flow/releases/tag/v0.3.0
[0.2.0]: https://github.com/laomou/lm-flow/releases/tag/v0.2.0
[0.1.1]: https://github.com/laomou/lm-flow/releases/tag/v0.1.1
[0.1.0]: https://pypi.org/project/lm-lmflow/0.1.0/
