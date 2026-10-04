# Production cutover measurements

Measured on 2026-10-04 on a local arm64 macOS 27.0 (26A428) host. Baseline: the Nix-packaged
compiler at `eeb5655`, the last commit before production cutover. Final path:
`a169308` plus the async-WIT suspension diagnostic committed with this report.
Both artifacts run in the same Wasmtime 49.0.2 engine, with P2 and P3 bindings
registered together. The Rust harness uses Cargo's default test profile;
Cranelift compiles both guests with its default settings. These are local
comparisons, not production capacity estimates.

[The harness](tests/cutover_measurement.rs) compiles the same independently written
TypeScript with each compiler and checks every result. It excludes compilation
and instantiation from timing, warms each instance with five calls, then measures
five batches of 50 serial calls. Timings include host calls, canonical argument
and result copying, and output comparison. Raw batch samples are saved in
[cutover-2026-10-04.json](measurements/cutover-2026-10-04.json).

| Workload | Pipeline | Stripped component | Linear memory after warmup → after samples | Median ms/call | Payload MiB/s |
| --- | --- | ---: | ---: | ---: | ---: |
| Text | Legacy | 206,174 B | 3,200 → 4,160 KiB | 0.05588 | 69.90 |
| Text | P3 | 9,927 B | 64 → 64 KiB | 0.02089 | 187.04 |
| File I/O | Legacy | 224,495 B | 2,368 → 2,368 KiB | 1.40725 | 44.41 |
| File I/O | P3 | 25,319 B | 256 → 256 KiB | 0.91044 | 68.65 |

Text starts with 4,096 ASCII bytes and appends one character 64 times. File I/O
reads 65,536 ASCII UTF-8 bytes, overwrites a sibling file, and returns the text.
The file is cached; this does not measure physical disk bandwidth. Throughput
counts one input payload per call, not all internal copies or both read and write
bytes. ASCII keeps both compilers' string semantics comparable; Unicode correctness
is covered separately by scalar-text and component boundary tests.

Linear memory counts successful Wasm memory allocations/growth, summed across the
component's core memories. It excludes host RSS, JIT code, reserved address space,
and task stacks. No outstanding native tasks or host resources remain after either
workload. P3 storage remains flat during the measured calls. Legacy text storage
grows after warmup; the table makes no claim about its eventual plateau.

For these workloads P3 reduces component size by 95.2% and 88.7%, respectively,
and median latency by 62.6% and 35.3%. No measured regression requires optimization.
The input size, serial execution, warm cache, and debug host profile limit how
broadly these figures apply. Deferred concurrency and additional library surfaces
are not benchmarked.

The measurement exposed and fixed a contract gap: synchronous WIT exports could
reach suspending P3 operations and trap when the host needed to block. The compiler
now follows source calls and requires `async func` for those exports, including
filesystem/console operations behind helpers. Pure synchronous exports remain
valid even alongside unrelated async exports. The P3 I/O benchmark therefore uses
an async WIT export; legacy I/O retains its P2 synchronous contract. A legacy
`slice` stub failed output validation and is excluded from comparisons.

## Reproduce

Build the baseline compiler from `eeb5655` in a separate checkout using its pinned
Nix flake. Keep that executable and enter the current compiler's `nix develop`:

```sh
PERRY_BASELINE_COMPILER=/absolute/path/to/baseline/bin/perry-wit \
PERRY_MEASUREMENT_OUTPUT=/tmp/cutover.json \
  cargo test --test cutover_measurement -- --ignored --nocapture
```

The baseline executable used here was
`/nix/store/84q6mqyk6qz0w0kvfcz3m80vxrvky29d-perry-wit-0.1.0/bin/perry-wit`.
The harness creates temporary WIT packages using the official P2/P3 interfaces;
no runner source or WIT is used. Default tests skip this opt-in measurement.
