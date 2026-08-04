# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Three artifacts ship from one version number: the `lmflow` crate on crates.io, the
`lm-lmflow` wheel on PyPI (imported as `lmflow`), and the per-platform native SDK attached
to each GitHub Release.

## [Unreleased]

### Added

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

[0.2.0]: https://github.com/laomou/lm-flow/releases/tag/v0.2.0
[0.1.1]: https://github.com/laomou/lm-flow/releases/tag/v0.1.1
[0.1.0]: https://pypi.org/project/lm-lmflow/0.1.0/
