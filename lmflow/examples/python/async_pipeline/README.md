# Asyncio production pipeline

This example combines:

- a bundled C++ kernel on a two-thread pool;
- a Python kernel on `DelegatingExecutor`, driven by the event-loop thread;
- typed asynchronous output events;
- `Graph.run_async(timeout=..., cancel_grace=...)`;
- deterministic cleanup on timeout, task cancellation, or Ctrl-C.

Run from the repository root after installing LMFlow:

```bash
python -m pip install -v .
python lmflow/examples/python/async_pipeline/async_pipeline.py
```

Expected output:

```text
processed 5 packets: [1, 11, 21, 31, 41]
```

`run_async()` uses the engine wakeup callback rather than a polling loop. When
the asyncio task is cancelled it calls `graph.cancel()`, waits up to
`cancel_grace`, and then re-raises `CancelledError`. The surrounding `with`
statement remains the final synchronous cleanup guard.
