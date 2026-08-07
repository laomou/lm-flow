# Production route + async events

This example combines the production-facing pieces of LMFlow:

- ordinary Python kernels and an engine-provided `type: route` node;
- packet metadata for confidence/category routing;
- `async for` typed output events;
- bounded queues and graceful cancellation;
- periodic diagnostic DOT/SVG snapshots.

Run from the repository root after building the Python extension:

```sh
PYTHONPATH=.pydeps:lmflow/python python3 \
  lmflow/examples/python/production_route_async/production_route_async.py
```

Install Graphviz separately if SVG snapshots are desired.
