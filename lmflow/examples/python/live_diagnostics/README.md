# Live diagnostics snapshots

This example exports a running graph every 100 ms with:

- Top-5 node and input-port hotspot rankings
- Interval throughput, latency, backpressure, and drop deltas
- Purple upstream pressure propagation paths
- DOT output on every system, plus SVG when Graphviz is installed

Run from the repository root:

```bash
python -m pip install -v .
python lmflow/examples/python/live_diagnostics/live_diagnostics.py
```

Snapshots are written to `lmflow-diagnostics/`.
