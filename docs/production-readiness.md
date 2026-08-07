# Production readiness

## Performance benchmarks

Run the Rust benchmark suites from `lmflow/core`:

```sh
cargo bench --bench packet
cargo bench --bench dispatch
cargo bench --bench throughput
cargo bench --bench route_metadata
```

`route_metadata` measures:

- metadata lookup, key enumeration, and metadata-only copy-on-write;
- `type: route` first/all mode throughput;
- route predicate cost as rule count grows from 1 to 16.

Use Criterion's saved reports for regression tracking. Compare the same profile, CPU governor,
thread count, and payload sizes; benchmark numbers from different machines are not directly
comparable.

The production Python example also exercises the complete path:

```sh
PYTHONPATH=.pydeps:lmflow/python python3 \
  lmflow/examples/python/production_route_async/production_route_async.py
```

## Cross-language metadata API

| Capability | Rust | C | C++ | Python |
| --- | --- | --- | --- | --- |
| Set scalar/string | `Packet::set_metadata` | `lmflow_packet_set_metadata_*` | `Packet::SetMetadata` | `Packet.set_metadata` |
| Read scalar/string | `metadata_value` | `lmflow_packet_metadata_*` | `Packet::Metadata` | `Packet.metadata` |
| Test key existence | `has_metadata` | `lmflow_packet_has_metadata` | `Packet::HasMetadata` | `Packet.has_metadata` |
| Remove key | `remove_metadata` | `lmflow_packet_remove_metadata` | `Packet::RemoveMetadata` | `Packet.remove_metadata` |
| Enumerate keys | `metadata_keys` | `metadata_count/key_at` | `Packet::MetadataKeys` | `Packet.metadata_keys` |

The C ABI `LMFlowPacket` layout is unchanged. Metadata is stored in a separate copy-on-write map;
adding, removing, or enumerating metadata never copies the payload.

Supported values are bool, signed 64-bit integer, double, and UTF-8 string. Metadata setters require
an owned engine packet. Borrowed input packets should be copied or taken before mutation.

## Diagnostics and operation

Use static validation before deployment:

```sh
lmflow check-config graph.yaml --json
lmflow check-config graph.yaml --dot > plan.dot
```

Use runtime diagnostics during a canary run:

```python
graph.to_dot(lmflow.DotView.DIAGNOSTICS)
```

The diagnostic graph includes queue/backpressure state, executor saturation, route rule hit counts,
default/unmatched/drop/error counters, missing metadata observations, and predicate evaluation errors.

Recommended production checks:

1. Keep `input_queues.packets` bounded for every real-time branch.
2. Set `stats: basic` by default; use `stats: full` for latency/CoW investigations.
3. Add a `default` route or explicitly choose `unmatched: drop/error`.
4. Export diagnostic DOT at startup, during a canary, and after shutdown.
5. Close graph inputs before `wait_done`; use `run_async`/`events` in asyncio applications.

## Failure triage

- **High queue depth / blocked ports**: increase downstream capacity only after checking executor
  saturation; otherwise reduce input rate or use a lossy `fixed_size` policy.
- **Route unmatched grows**: inspect metadata keys and types with `Packet.metadata_keys()` and
  compare the condition summary in `check-config --json`.
- **Predicate evaluation errors**: verify numeric comparisons use i64/f64 and `contains` uses strings.
- **Async task cancellation**: cancel the graph, wait for the configured grace period, then call
  `close()` as the synchronous final cleanup fallback.
- **Cross-language metadata mismatch**: keep values within the four language-neutral types and avoid
  host objects; the packet payload remains a separate zero-copy object.
