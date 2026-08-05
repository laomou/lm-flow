# Performance benchmarks

The benchmark suites are separated by responsibility so the pure Rust core does
not acquire a C++ build path.

## Pure Rust engine

Run from `lmflow/core`:

```sh
cargo bench --bench dispatch
cargo bench --bench throughput
cargo bench --bench packet
```

- `dispatch`: scheduler and executor dispatch cost without Poller overhead.
- `throughput`: public Rust `Input -> Graph -> Poller` throughput and large-payload
  zero-copy forwarding.
- `packet`: reference-counted clone and copy-on-write cost by payload size.

Criterion reports are written to `lmflow/core/target/criterion/`.

## Complete native SDK

The native benchmark links the same `lmflow::lmflow` target used by C++ consumers.
It is disabled by default and requires the bundled kernels:

```sh
cmake -S . -B build-bench \
  -DCMAKE_BUILD_TYPE=Release \
  -DLMFLOW_BUILD_TESTS=OFF \
  -DLMFLOW_BUILD_BENCHMARKS=ON \
  -DLMFLOW_BUILD_KERNELS=ON
cmake --build build-bench --target lmflow_native_throughput --parallel
./build-bench/lmflow/benchmarks/lmflow_native_throughput 10000
```

It covers C ABI round trips, large-buffer forwarding, and the payload-dependent
cost of `InvertKernel`.

## Python binding

Build or install the wheel, then run:

```sh
python lmflow/benchmarks/python_throughput.py --iterations 5000
```

This measures the public Python API, including Python/NumPy packet conversion.
Compare it with the native benchmark to estimate binding and conversion overhead.

Benchmarks are intentionally not CI pass/fail gates: hosted-runner timing is too
noisy for reliable regression thresholds. CI compiles the benchmark targets; use
repeatable dedicated hardware for performance comparisons.
