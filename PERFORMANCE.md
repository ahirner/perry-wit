# Performance measurements

## Production snapshot at `8f916eb`

Measured on 2026-10-04 using the pinned Wasmtime 49.0.2 on arm64 macOS 27.0,
with the test profile and default Cranelift settings. Both harnesses checked every
result and required empty native task state after calls. The table uses the median
of five batches of 50 calls after five warmup calls. [Raw samples](measurements/production-8f916eb.json)
record the compiler revision and every batch.

| Workload | Stripped component | Linear memory after warmup → after samples | Median time/call |
| --- | ---: | ---: | ---: |
| Text, 4 KiB | 10,211 B | 64 → 64 KiB | 0.02311 ms |
| File I/O, 64 KiB | 25,420 B | 256 → 256 KiB | 0.91783 ms |
| Promise.all, 1 task | 13,906 B | 64 → 64 KiB | 0.09058 ms |
| Promise.all, 16 tasks | 13,906 B | 64 → 64 KiB | 1.16282 ms |
| Promise.all, 64 tasks | 13,906 B | 64 → 64 KiB | 5.29331 ms |
| Promise.all, 256 tasks | 13,906 B | 128 → 128 KiB | 41.78746 ms |

These are whole-call costs, including source execution, allocation, collection,
scheduling, canonical transport, and result validation. Linear memory remained
constant during sampling; host stacks, JIT code, and RSS are excluded. The file
workload uses cached data. Timing differs from the earlier runs below, and these
separate local samples do not isolate a particular compiler change.

## Owned async fan-out

[The scheduling harness](tests/async_measurement.rs) starts typed async functions,
each suspending once, joins them with `Promise.all`, and sums their ordered results.
It validates every result and checks that native task state is empty and linear
memory stops growing after warmup. It measures the entire call, including source
loops, guest allocation/collection, scheduling, canonical transport, and result
checking; it is not an isolated promise-reaction benchmark.

Local measurements on 2026-10-04 use arm64 macOS 27.0, Wasmtime 49.0.2, Cargo's test
profile, and default Cranelift settings. Each instance receives five warmup calls
and five batches of 50 calls. Compilation and instantiation are excluded. The
8 MiB limit applies to guest linear memory; host task stacks, JIT code, and RSS are
not measured. [Raw samples](measurements/async-2026-10-04.json) include the same
workload before and after indexing heap blocks during collection.

| Tasks per call | Median before heap index | Median with heap index | Linear memory after warmup and samples |
| ---: | ---: | ---: | ---: |
| 1 | 72.83 µs | 72.30 µs | 64 KiB |
| 16 | 946.40 µs | 911.68 µs | 64 KiB |
| 64 | 7.97 ms | 4.17 ms | 64 KiB |
| 256 | 265.95 ms | 32.59 ms | 128 KiB |

The indexed component is 13,947 bytes stripped, up from 13,728 bytes. The index
uses reserved heap-header storage without additional guest allocations. These
local samples show roughly an eightfold improvement at 256 tasks. Cost still
grows faster than the task count; the source loops, allocation scans, and tracing
remain part of that cost. The results do not establish performance for network
latency, cancellation, returned streams, or production host configurations.

Run in the pinned development shell:

```sh
PERRY_ASYNC_MEASUREMENT_OUTPUT=/tmp/async.json \
  cargo test --test async_measurement -- --ignored --nocapture
```

## Historical compiler comparison

Measured on 2026-10-04 on a local arm64 macOS 27.0 (26A428) host. Baseline: the Nix-packaged
compiler at `eeb5655`, the last commit before production cutover. Final path:
`a169308` plus the async-WIT suspension diagnostic committed with this report.
Both artifacts run in the same Wasmtime 49.0.2 engine, with P2 and P3 bindings
registered together. The Rust harness uses Cargo's default test profile;
Cranelift compiles both guests with its default settings. These are local
comparisons, not production capacity estimates.

The historical harness at `6bb5318:tests/cutover_measurement.rs` compiled the same
independently written TypeScript with each compiler and checked every result. It excludes compilation
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
and median latency by 62.6% and 35.3%. Neither of these workloads showed a regression.
The input size, serial execution, warm cache, and debug host profile limit how
broadly these figures apply. Concurrency and additional library surfaces were not benchmarked in this comparison.

The measurement exposed and fixed a contract gap: synchronous WIT exports could
reach suspending P3 operations and trap when the host needed to block. The compiler
now follows source calls and requires `async func` for those exports, including
filesystem/console operations behind helpers. Pure synchronous exports remain
valid even alongside unrelated async exports. The P3 I/O benchmark therefore uses
an async WIT export; legacy I/O retains its P2 synchronous contract. A legacy
`slice` stub failed output validation and is excluded from comparisons.

### Repeat component measurements

[The current harness](tests/component_measurement.rs) measures the production P3
compiler with the same text and file workloads:

```sh
PERRY_MEASUREMENT_OUTPUT=/tmp/components.json \
  cargo test --test component_measurement -- --ignored --nocapture
```

The historical comparison harness and its pinned environment are available at
`6bb5318`. Its baseline compiler came from `eeb5655`. Historical raw samples above
remain unchanged; current measurements do not rebuild or load the retired runtime.
