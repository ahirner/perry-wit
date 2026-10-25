# Property storage experiments

Fresh empty dictionaries now use the same frontend write and alias analysis as empty arrays. The unchanged upstream `bench_object_property.ts` compiles through the existing typed dictionary representation. JSON values keep the existing MIR heap provenance, including undeclared fields carried by typed parsed records.

## Reproduction

```sh
nix develop -c cargo build --locked -p perry-wit
nix develop -c python3 scripts/property-layouts/run.py --runs 7
```

The runner snapshots the compiler and records its hash, the template hash, repository revision, platform, runtime versions, every sample, and module sizes in `artifacts/property-layouts/results.json`. Generated sources, components and process logs live beside that report. `--fields`, `--objects`, `--runs`, `--compiler` and `--output` select other workloads.

Each process checks overwrites, missing keys, reversed insertion order, long keys with identical prefixes, empty strings, embedded NULs and Unicode collisions. It then resets shared metadata and warms up 100 objects. The timed section creates 10,000 objects, inserts computed keys and reads the first and last fields. Each checksum is checked against `fields * objects * objects - objects` in both Node and Wasmtime. Runs alternate engine order. Elapsed times come from the guest's millisecond clock; compilation and process startup are outside that interval. Whole-process peak RSS includes the engine and JIT and cannot measure object-layout size directly.

## Variants

| Variant | Change from the preceding variant |
| --- | --- |
| `Dictionary` | Existing `Record<string, number>` storage, with one allocated key/value record per entry. |
| `SeparateKeys` | Parallel key and value arrays; linear string lookup. |
| `CompactKeys` | Cache two 32-bit prefix words and the length; short Latin-1 keys compare exactly, other keys fall back to full string equality. |
| `InternedKeys` | Share the prefix table across objects and store numeric key IDs per object. |
| `SharedShapes` | Share ordered key-ID layouts and insertion transitions; each object retains its shape ID and values. |

These are guest-side algorithm probes. Their number arrays still use the compiler's numeric element representation, and their parallel metadata arrays add allocation and bounds-check overhead. Prefix construction uses `charCodeAt` loops. The interner scans a linear table, and shape reads scan key IDs without a property-site inline cache. A native packed key header, hash interner or cached shape-to-slot lookup needs a separate compiler/runtime measurement. Results from these probes cannot establish the performance of those implementations.

The [BBTree article](https://bbtree.xiangpeng.systems/post/) explores inlining key prefixes to reduce pointer chasing and accelerate comparisons. Interning adds a separate identity table. [Object shapes](https://bernsteinbear.com/blog/object-shapes/) share property layouts; a guarded lookup cache can then reuse field offsets. Keeping these mechanisms separate makes their costs measurable.

## Mixed arrays

The aggregate pass distinguishes no observations, known value types and unknown contents. It currently selects storage only for uniform writes. Different known write types remain distinct observations, but conditional-expression inference and interprocedural heterogeneous returns still need extension. Escaping aliases and unknown JSON sources remain conservative. This change does not implement mixed arrays.

The next design decision is how to represent mixed elements and execute their reads. [Storage strategies](https://tratt.net/laurie/research/pubs/html/bolz_diekmann_tratt__storage_strategies_for_collections_in_dynamically_typed_languages/) provide a precedent for specialized homogeneous storage with a general fallback. [V8 elements kinds](https://v8.dev/blog/elements-kinds) distinguish integer, double and general tagged storage. These designs motivate measuring packed storage with transitions alongside allocating a tagged representation from the start.

Index-sensitive narrowing is another independent question: proving that a read is numeric requires accounting for index relations, aliases, calls and intervening writes. A finite set of written types alone does not prove which type occupies a given index. Any mixed representation must preserve reference tracing, unknown JSON fields, and the supported semantics of addition, comparison and property access. Numeric indexing needs no property-key interner.

## Results

The unchanged upstream suite passes 23 of 24 benchmarks. `bench_object_property` produces `checksum:1999990000` in every run. Its three fresh-process samples are Node **9, 8, 9 ms** and Wasmtime **25, 25, 25 ms**. `bench_numeric_array_downgrade` remains rejected because its heterogeneous function result cannot yet be inferred.

Measurements use compiler commit `96e5dd43`, Node 26.10.0 and Wasmtime 50.0.0 on macOS 27 arm64. The compiler executable is frozen for both campaigns. The upstream source is Perry `be6740f46e85db2ab5524bc11cd5af53f29f7846`; the object benchmark's SHA-256 is `3771014335991424de18e4bb9e6d54471a75ef920b9f0fe995e5f0d255d84fda`.

The table below reports median Wasmtime guest milliseconds over seven fresh processes for each layout. All prototypes use the same wrapper and workload, so compare them with the `Dictionary` row; the upstream benchmark above has its original source and separate warmup.

| Fields | Order | Dictionary | Separate keys | Compact prefixes | Interned keys | Shared shapes |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 8 | ordered | 8 | 8 | 14 | 14 | 14 |
| 8 | alternating | 8 | 8 | 15 | 14 | 15 |
| 20 | ordered | 24 | 22 | 39 | 39 | 35 |
| 20 | alternating | 26 | 22 | 40 | 39 | 36 |
| 64 | ordered | 124 | 100 | 161 | 193 | 164 |
| 64 | alternating | 127 | 101 | 162 | 192 | 168 |

These Wasmtime probes favor parallel key/value arrays: the 20-field ordered case falls from 24 to 22 ms, and the 64-field case from 124 to 100 ms. The 8-field case is tied at 8 ms. Shared shapes improve on the linear interner at 20 fields (39 to 35 ms) but remain slower than the dictionary baseline. Neither prefix caching nor the interner wins against that baseline in this experiment. Node has different tradeoffs: at 64 fields its separate-array probe is slower than its dictionary probe (49 versus 28 ms).

The [recorded samples](measurements.json) include all seven timings per engine, provenance and the unchanged upstream result. At 20 ordered fields, Wasmtime ranges are dictionary 24–25 ms, separate arrays 21–23 ms, prefixes 39–40 ms, interning 39 ms, and shapes 34–35 ms. Millisecond resolution limits the smaller comparisons.

## Compiler arrays

The compiler now splits eligible private dictionaries into ordinary typed key and value arrays after bounds proofs. Paired appends preserve equal lengths; field reads and overwrites use the proven index directly. Each appended entry must be freshly allocated in the append's block, with no observable entry identity. Direct serialization, calls, escaping entries and structural edits retain the existing entry representation. Referenced values still use the existing array heap analysis, JSON openness facts and collector.

The unchanged upstream suite passes **23/24** with both frozen compilers. The existing `bench_numeric_array_downgrade` inference failure remains. Only `bench_object_property` changes its emitted component; the other 22 passing benchmarks produce byte-identical Wasm. Its component grows from 36,494 to 36,784 bytes (+0.8%).

Nine fresh-process object benchmark comparisons alternate baseline/candidate order, with a fresh Node reference each round. Every output passes upstream verification with checksum `1999990000`.

| Build | Guest milliseconds | Median |
| --- | --- | ---: |
| Baseline | 29, 27, 26, 29, 27, 27, 26, 27, 26 | 27 ms |
| Separate arrays | 24, 26, 25, 28, 26, 25, 24, 25, 25 | 25 ms |
| Node | 10, 11, 10, 10, 10, 9, 10, 10, 10 | 10 ms |

The candidate is faster in all nine pairs; the median falls **7.4%**. These are millisecond-resolution measurements on a shared host, so the paired comparison is more useful than comparing absolute times with the earlier guest-prototype campaign. The separate three-run full-suite medians are 30 → 27 ms for this benchmark; timing variation elsewhere occurs despite identical emitted code.

Measurements use Node 26.10.0 and Wasmtime 50.0.0 on macOS arm64, with the same upstream revision and source hash recorded above. Baseline production source matches `b443ff73`; its frozen executable was built at `96e5dd43`, before documentation and test-only changes. [Compiler measurements](compiler-columns.json) record both executable hashes, all paired samples, per-benchmark timings, component hashes and conformance results. Local logs and frozen executables are in `artifacts/dictionary-columns/`.

JSON, array and object conformance all pass: six contracts, 208 executions, with 32 generated cases per selected contract where supported. Extended witnesses cover growth, overwrites, missing keys, enumeration snapshots, aliased record payloads and nested undeclared JSON fields. The integration oracle also runs with forced garbage collection. Workspace tests and strict all-target/all-feature Clippy pass.

```sh
nix develop -c cargo build --locked -p perry-wit -p perry-conformance
nix run .#bench-perry
nix develop -c target/debug/perry-conformance check --select ecma.array --cases 32
nix develop -c target/debug/perry-conformance check --select ecma.json --cases 32
nix develop -c target/debug/perry-conformance check --select ecma.object --cases 32
```

The `bench-perry` Nix app sets `BENCH_PERRY_SOURCE` and `BENCH_PERRY_REVISION` for the benchmark runner. For comparisons, freeze each compiler, prepend its directory to `PATH`, and run the unchanged runner from separate output directories. Repeat the resulting object components with alternating build order and upstream's stdout verifier. Compilation and process startup stay outside the guest timer.

Compact prefixes, key interning and shapes remain separate experiments. Their compiler implementations must preserve key equality, insertion/enumeration order, aliases and reference tracing. Mixed arrays still need an explicit element representation and read semantics; this layout change does not choose a heterogeneous inference strategy.

The ownership cleanup resolves field accesses to indices before rewriting, borrows field names, removes the record pass's name map and reads allocation operands in place. Dictionary eligibility is planned once per rewrite. Both passes keep only their crate-level entry points; the shared field-access helper is private. Eligibility and identity-safety tests remain, while duplicate assertions about the exact emitted layout are removed.

A [nine-run recheck](quality-columns.json) measured the original entries at **25 ms** median, columns before cleanup at **22 ms**, and columns after cleanup at **22 ms**. The two column compilers emit byte-identical object benchmark components. This confirms the layout gain (12% in this campaign); it does not claim a guest speedup from the Rust ownership cleanup. Workspace tests and strict Clippy pass.
