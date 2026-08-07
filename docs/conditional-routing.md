# Conditional routing

`type: route` is an engine-provided node with one input and multiple outputs. Unlike a normal
`kernel:` node, it does not require kernel registration.

```yaml
stats: full

input_ports: [camera]
output_ports: [tracked, reviewed]

nodes:
  - name: preprocess
    kernel: PreprocessKernel
    input_ports: [camera]
    output_ports: [normalized]

  - name: detector
    kernel: DetectorKernel
    input_ports: [normalized]
    output_ports: [detections]

  - name: confidence_router
    type: route
    input_ports: [detections]
    output_ports: [high, medium, rejected]
    mode: first
    unmatched: rejected
    routes:
      - to: high
        when:
          all:
            - { metadata: confidence, op: gte, value: 0.8 }
            - { metadata: category, op: eq, value: person }
      - to: medium
        when: { metadata: confidence, op: gte, value: 0.5 }
      - to: rejected
        default: true

  - name: tracker
    kernel: TrackerKernel
    input_ports: [high]
    output_ports: [tracked]

  - name: reviewer
    kernel: ReviewKernel
    input_ports: [medium]
    output_ports: [reviewed]

  - name: rejected_logger
    kernel: DropLoggerKernel
    input_ports: [rejected]
```

`mode: first` emits only to the first matching rule. `mode: all` emits a reference-counted packet
clone to every matching rule; the payload remains zero-copy. `unmatched` accepts `drop`, `error`,
or an output port name. A `default: true` rule is evaluated only after all conditional rules miss.

Conditions support metadata and timestamps:

```yaml
when:
  any:
    - { metadata: category, op: contains, value: person }
    - not:
        timestamp: { op: lt, value: 1000 }
```

Supported operators are `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `exists`, and string `contains`.
Metadata values are restricted to bool, i64, f64, and UTF-8 strings.

## Packet metadata

Rust:

```rust
let packet = Packet::from_i64(42)
    .with_metadata("confidence", 0.92)
    .with_metadata("category", "person");
```

C++:

```cpp
auto packet = lmflow::Packet::FromI64(42);
packet.SetMetadata("confidence", 0.92);
packet.SetMetadata("category", "person");
```

Python:

```python
packet = lmflow.Packet.from_int(42)
packet.set_metadata("confidence", 0.92)
packet.set_metadata("category", "person")
```

The metadata map uses copy-on-write independently of the payload. Adding metadata or routing a
packet does not copy the payload.

## Diagnostics

`lmflow check-config graph.yaml --dot` and `--json` include the route mode, rule order, condition
summary, destinations, and static warnings for shadowed or duplicate rules.

Runtime diagnostic DOT additionally shows per-rule evaluated/matched/emitted counts, default and
unmatched counts, drops, errors, missing metadata observations, and predicate evaluation errors:

```rust
std::fs::write(
    "runtime.dot",
    graph.to_dot_with_view(lmflow::DotView::Diagnostics),
)?;
```

Render it with Graphviz:

```sh
dot -Tsvg runtime.dot -o runtime.svg
```
