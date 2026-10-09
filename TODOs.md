# TODOs: MIR refactor, scalable memory, stackless async

Status: in progress. Plan from 2026-10-08; the baseline is `3dbf57a`.

- **2026-10-09: the legacy WAFFLE backend is deleted.** There is no dual backend: the MIR pipeline is the only compiler, and features return through P6.2. That covers about 40k lines under `src/waffle_backend` and 20 test suites, which are kept in git history at `45e8788`.
- **Current continuation: byte consumers**, following the core JSON, typed storage, enumeration and string checkpoints. TextEncoder/TextDecoder now use byte views; filesystem metadata and directory mutations are restored. Continue with file transfers and directory listing, then streams and fetch. Core strings use UTF-8 scalar semantics; JSON is compact and callback-free. The accepted profile below supersedes the earlier reviver/replacer and microtask-ordering checkpoints. General non-JSON dictionary views, remaining typed operations, CLI formatting and promise combinators remain required. This handoff ordering takes precedence over registry order in P6.2.
- The bonus goal, removing WAFFLE (P4.4), is done.
- Coverage is tracked by `crates/conformance/passing.txt`.
Background: `claude/compiler-critique-2026-10-08.md` and `claude/refactor-design-notes.md` in the perry-wit project.

## Goals

1. **Replace hand-rolled lowering with a typed middle layer.** Pipeline: Perry HIR → pliron MIR → reused and owned passes → `wasm-encoder`. Removing WAFFLE entirely is a bonus goal.
2. **Make memory management scale.** Use a static-liveness-first memory protocol, influenced by Zena:
   - Most values never allocate.
   - Most of what does allocate is freed in bulk by scope.
   - Only state that can be retained goes through a simple, scalable tracing collector.
   - The design stays open to WasmGC, which is decided later (D6).
3. **Trade dynamic generality for static liveness.** Reject language forms that defeat static lifetime reasoning instead of supporting them at runtime.
4. **Remove all Component Model thread features.** Async becomes stackless state machines on the P3 callback-lift ABI.

Non-goals for this refactor: new TypeScript features, a new frontend or parser, and a WasmGC backend (only a spike, P8).

## Decisions

### D1. MIR in pliron

- Pin `pliron` 0.19.0 from `github.com/pliron-org/pliron`, commit `6ff5b3c` (2026-10-06). Apache-2.0, edition 2024, with `default-features = false` where possible.
- Do not depend on `pliron-llvm`: its default feature pulls in `llvm-sys`, which breaks the LLVM-free audit.
- Reused from pliron:
  - ops, types, attributes and interfaces (derive macros);
  - verifiers, the rewriter and dialect conversion (`irbuild/dialect_conversion.rs`);
  - the pass manager and liveness analysis (`analyses/liveness.rs`);
  - SCCP, DCE, `simplify_cfg`, and `mem2reg` with `PromotableAllocationInterface` / `PromotableOpInterface`.
- Owned by perry-wit (or contributed upstream):
  - inliner;
  - interprocedural escape summaries;
  - scalar replacement of aggregates (SROA);
  - integer ranges;
  - shapes;
  - tier classification;
  - async split;
  - root planning;
  - Wasm emission.

### D2. Emit Wasm with `wasm-encoder` and remove WAFFLE

- Lower MIR through a `wasm` dialect, then a stackifier, then `wasm-encoder`.
- The reference design is Zena's `packages/zena-compiler/zena/lib/codegen/ir/emit.zena` (about 2.4k lines). It uses:
  - Ramsey's dominator-based "Beyond Relooper" stackifier, which needs a reducible CFG;
  - copy coalescing of block arguments, ordering of parallel moves with cycle temporaries, and stack scheduling.
- WAFFLE today emits Perry's code and parses the runtime `.wat` files into SSA.
  - Helper linking already uses `wasm_encoder::reencode` (`link.rs`) and stays.
  - The runtime `.wat` files move to Rust crates (P5), so WAFFLE has no remaining job.

### D3. Memory protocol

Every allocation site is assigned a **tier** by static analysis on MIR. Each tier has one reclamation rule.

| Tier | What qualifies | Representation (linear memory) | Reclaimed |
|---|---|---|---|
| `scalar` | Does not escape after inlining. All uses are field reads and writes. | SSA values, block parameters, multi-value returns | Never allocated |
| `frame` | Does not escape the function activation and is not live across `await`. | Per-task bump stack | When the function exits (pop) |
| `task` | Escapes the function but not the export task. Covers async frames, promises, readers, request/response temporaries. | Per-task region made of bump chunks; objects carry shape headers | In bulk at `task.return` / `task.cancel` (O(chunks)) |
| `instance` | May be reachable from module state across invocations, or fails the loop-growth rule. | Size-class pages, non-moving | Precise mark-sweep (below) |
| `buffer` | Canonical ABI return areas, list and string lowering, stream and BYOB byte storage; region chunks. | `talc` 5.1 (decided below) | Explicit owner: call, transfer acknowledgement, or stream object |

**Allocator decision (2026-10-08): `talc` 5.1, `WasmDynamicTalc`.** Measured with `bench/allocators/run.sh` (wasm32, `opt-level=z`, LTO; Node timings, ±30 % run to run):

| Allocator | Code bytes | Stream 8 KiB | Grow 4 MiB (doubling) | Grow 4 MiB (+8 KiB exact) | Churn (16 B–64 KiB, 1k live) | 64 KiB chunk regions |
|---|---:|---:|---:|---:|---:|---:|
| talc 5.1.1 | 1,876 | 38–49 ns | 0.5 µs, 9.3 MiB | 23 ns, 7.4 MiB | 69–84 ns, 5.7 MiB | 1.7–2.6 µs, 5.9 MiB |
| dlmalloc 0.2.14 | 6,724 | 25–156 ns | 0.4–0.6 µs, 9.3 MiB | 18–39 ns, 7.4 MiB | 94–146 ns, 5.2 MiB | 2.0–2.1 µs, 5.8 MiB |
| rlsf 0.2.3 (small / full) | 1,351 / 2,505 | 19–25 ns | 12–14 µs | 12–200 µs, 87–273 MiB | 61–91 ns | 1.5–3.8 µs, 10.6 MiB |
| lol_alloc 0.4.1 | 680 | 32 ns | 13 µs | 150–171 µs | 700–900 ns | 8.5–9 µs |

- talc is 3.6× smaller than dlmalloc and as fast or faster everywhere it matters. Its in-place `realloc` keeps list and string growth linear.
- rlsf copies on every grow and fragments badly under region churn. lol_alloc is O(n) per free.
- talc uses about 10 % more memory than dlmalloc under random churn. That is acceptable because hot paths do not go through it: regions recycle their own chunks, and the runtime will recycle stream buffers itself.

Tier rules:

- **Escape lattice:** `scalar < frame < task < instance`.
- **Storing into an object raises the stored value to at least that object's tier.** Storing into module state raises it to `instance`. A `task` object therefore never points into `frame`, and an `instance` object never points into `task`.
- **Loop-growth rule.** An allocation site inside a loop is promoted to `instance` unless its tier is proven `frame` or lower when either of these holds:
  - Its object can be dropped by a loop-carried reassignment, for example `state = {...state, x}`.
  - It overwrites a reference slot in a container.

  This keeps `task` regions bounded by real live data and leaves dynamic liveness to the collector.
- Each task gets its own region, so overlapping export tasks on one instance do not block reclamation. This fixes nix-wit's global-cursor limitation.

`instance` collector (linear memory):

- **Object header:** 4 bytes holding the shape id. Mark bits sit in per-page side bitmaps.
- **Tracing:** compiler-emitted pointer maps per shape, as a data segment.
- **No interior pointers.** A byte view is a `(buffer-owner, offset, length)` triple.
- **Mark phase:** an explicit mark stack held in `buffer` storage. No fixpoint rescans and no `find` walks.
- **Roots:**
  - module globals;
  - `task` regions, restricted to objects whose shape has reference fields, which are listed per region at allocation;
  - shadow-stack spills at may-collect call sites, containing only the `instance` references live at that point according to pliron liveness.
- **Safepoints:** only `instance` allocation sites and calls that may reach one, from an interprocedural summary. No backedge polling.
- **Trigger:** collect when bytes allocated since the last GC exceed `max(256 KiB, live bytes after the last GC)`. Sweep lazily per page.
- **No write barriers.** The collector is not generational or incremental. The callback ABI keeps execution single-threaded per instance.

Zena influences:

- An `await` allocates nothing: the frame is the waiter and the promise.
- Escape summaries are per parameter and computed to a fixpoint, followed by inline + SROA.
- Interfaces are fat pointers.
- Synchronous steps return inline ready/pending results without allocating.

nix-wit influence: invocation-scoped regions. Here they are per-task and statically bounded.

### D4. Language subset (static liveness first)

These restrictions are added to README "Semantics", each with a diagnostic and a registered rejection contract:

- **R1 Closed shapes.**
  - Object literals, interfaces and type aliases have fixed field sets.
  - No undeclared property addition or `Object.assign` into existing records. Declared optional fields may be inserted/deleted; required fields cannot be deleted.
  - Spread creates a new shape.
  - `Record<string, T>` and index signatures become an explicit dictionary type, not a record.
- **R2 Structural widening goes through interface views.**
  - A wider record used where a narrower type is expected becomes a fat pointer `(object, field-offset table)`, emitted per (shape, interface) pair.
  - Identity and mutation of the existing fields are preserved.
- **R3 Async values cannot outlive their task.** Promises, readers, streams, `Response`/`Request` bodies and `AbortController` cannot be stored in module state or `instance` objects. Dangling work at task end already traps.
- **R4 Module state** consists of explicitly typed top-level bindings only. Values assigned to it are `instance`.
- **R5 Closures stay unsupported**, except non-escaping arrow arguments to known array and promise builtins. Those are always inlined and never allocated.
- **R6 No `any`.** Untyped data is a `JsonValue` variant tree.
- **R7 Cycles are allowed only in `instance`.** The collector handles them. `task` cycles are freed in bulk anyway.
- **R8 Arrays are dense; misses are `undefined`, failures are exceptions.**
  - A read outside `[0, length)` or at a non-integral index returns `undefined`, as in ECMAScript. In MIR the read has type `perry.optional<T>`.
  - `undefined` is a reserved NaN payload for numbers and a null reference for references, so the bounds check is one compare and a select.
  - Arithmetic and ordering coerce `undefined` to `NaN` (`perry.to_number`), concatenation to `"undefined"` (`perry.to_string`), and `=== undefined` is a definedness test.
  - A typed boundary that requires `T` (parameter, return, field, element, property access) unwraps with `perry.unwrap`, which throws a catchable `TypeError` on `undefined`.
  - A write at `length` appends. Any other out-of-range or non-integral write throws a catchable `RangeError`, because holes are not representable.
  - Nothing in the subset traps. Wasm traps cannot be caught by `try`/`catch`, so every subset failure is an ECMAScript exception instead.
  - P3.5 removes the check, the optional wrapper and the unwrap where the index is proven in range, so hot loops pay nothing.

**Accepted compatibility profile (2026-10-09, restored from main):**

- Uncontended settled awaits advance inline; synthetic ECMAScript microtask FIFO ordering is not required. Pending host operations and overlapping export tasks remain supported.
- JSON revivers, function/property-list replacers and indentation are deferred and diagnosed at compile time. Ordinary compact parse/stringify and typed narrowing stay in the core ratchet.
- Strings are UTF-8 and indexed by Unicode scalars. Node differential generation is restricted to BMP scalars; astral behavior has a separate scalar oracle. Reject charCodeAt/fromCharCode, lone-surrogate emulation and dynamic RegExp. Restore only bounded literal DFA regexes with zero guest allocations.
- `accepted_deviation` records reasons in the conformance catalog/report. The four historical contracts are `exempt`, distinct from `pending` migration work, and may not appear in `passing.txt`. `ecma.subset.exclusions` verifies the compile-time boundary. Core arithmetic, strings, dense arrays, records, compact JSON, UTC dates, streams and WASI I/O remain required.

### D5. Stackless async on the P3 callback ABI

Facts checked against component-model main `6c48079`, wasm-tools v1.261.0 and Wasmtime v49.0.2:

- **The export already uses `[async-lift]` plus `[callback][async-lift]`.** Internally it spawns CM threads (`[thread-new-indirect-v0]`, `[thread-yield]`, `[thread-suspend]`, `[thread-resume-later]`, `[thread-*-then-promote]`). All of these go.
- **Callback return codes:** the low 4 bits of the packed i32 are the code: EXIT=0, YIELD=1, WAIT=2. Bits `>>4` hold the waitable-set index. There is no POLL code.
- **Callback signature and events:** `(event, p1, p2) -> i32`.
  - Events: NONE=0, SUBTASK=1, STREAM_READ=2, STREAM_WRITE=3, FUTURE_READ=4, FUTURE_WRITE=5, TASK_CANCELLED=6.
  - Stream and future payload: `result | count<<4`, where COMPLETED=0, DROPPED=1, CANCELLED=2.
  - Subtask states: STARTING=0, STARTED=1, RETURNED=2, CANCELLED_BEFORE_STARTED=3, CANCELLED_BEFORE_RETURNED=4.
- **Builtins allowed with only the default `component-model-async`:**
  - `waitable-set.new/wait/poll/drop`, `waitable.join`;
  - `subtask.drop` and **sync** `subtask.cancel`;
  - `stream.*` / `future.*` `new`, **async** `read/write`, **sync** `cancel-*`, `drop-*`;
  - `task.return`, `task.cancel`, `backpressure.inc/dec`;
  - `context.get/set` (slot 0, i32 only);
  - `thread.yield`.
- **Needs `more-async-builtins`:** async `subtask.cancel`, sync `stream`/`future` read/write, and async `cancel-*`. Do not use any of them.
- **WASI 0.3 entry points:** `wasi:cli/run` is `run: async func() -> result` and `wasi:http/handler` is `handle: async func(request) -> result<response, error-code>`. Both are callback-lifted, and CLI no longer needs extra flags.
- **Required Wasmtime ≥ 49.0.2** because of the callback result-count fix GHSA-32h6-97mm-8q3c.
- **Version note:** Wasmtime 49 pins wasm-tools crates at 0.258.3, while perry-wit uses 0.261. The `cancellable` byte encoding is still compatible.
- **jco** implements the callback driver loop (YIELD/WAIT). Its thread trampolines are `todo!()`, so removing threads helps portability. End-to-end P3 behaviour in jco has not been verified.

### D6. WasmGC is deferred and gated by P8

- MIR `heap.alloc` carries a tier attribute, so both lowerings stay possible.
- Under WasmGC, `frame`, `task` and `instance` become `struct.new` / `array.new`. `buffer` stays in linear memory, and regions and shadow stacks disappear.

## Target pipeline

```text
TS source ─ SWC ─ Perry HIR (external, pinned 9e29f59)
   └─ frontend: bindings, WIT binding, command adapter, type table (memoized), subset checks
MIR (pliron dialects: perry.core, perry.heap, perry.async, perry.canon, perry.intrinsic)
   └─ passes: inline → SCCP/DCE/simplify_cfg → escape → SROA/mem2reg → int-range
              → shape/iface → tier → async-split → canon-lower → root-plan
wasm dialect ─ stackifier + local alloc ─ wasm-encoder core module
   └─ link.rs (reencode): perry-runtime.wasm + json/time/text/fetch/number/search helpers
   └─ wit-component ComponentEncoder (unchanged)
```

## Work items

The phases run in order. Items within a phase can run in parallel unless they are marked with `→`. Each item is complete when its *Done when* line holds.

### P0. Baselines and guards (on the current backend)

- [x] **P0.1 Scaling contracts.**
  - Register the runtime scaling contracts:
    - push n records then sum;
    - push n numbers;
    - a temporary record per iteration;
    - string append;
    - for each, time(4n)/time(n) must be ≤ 6.
  - Register a compile-time contract for nested property chains: depth 32 must compile in under 1 s.
  - Make allocator entries per iteration a reported metric.
  - *Done when:* the contracts exist and fail on `3dbf57a`, which documents the bugs. Mark them expected-fail on the legacy backend.
  - *Status (2026-10-08, on `f5c8bff`):*
    - The ratio uses guest **fuel**, not time, so it is deterministic: warm-up n, then n and 4n, on one instance.
    - Implemented in `crates/conformance/src/scaling.rs` as `wasi.execution.scaling.*`, with per-call fuel recorded in `Outcome::Values::fuel_used`.
    - `scaling::KNOWN_FAILURES` gives a `known_failure` status that does not fail the campaign; a listed contract that passes does fail it.
    - Known failures: `retained_records` and `number_push` (fuel exhausted at n=512).
    - Passing: `temporary_records` at 3.97× and `string_append` at 4.18×.
    - The compile-time guard is `tests/compile_scaling_test.rs`. Depth 32 must compile within 10 s; the test is `#[ignore]` until P1.1 lands.
    - The allocator-entries metric moves to P0.2.
- [ ] **P0.2 Benchmark baseline.**
  - `scripts/compare_components.mjs` and `tests/component_measurement.rs` measured legacy-only workloads and were deleted with the legacy backend. Rebuild the harness on the MIR pipeline as streams and HTTP return (P6.2 groups 8–9). Include the critique microbenchmarks, component sizes for all examples, and committed linear-memory peaks.
  - AssemblyScript comparisons stay out of the repo. Run them by hand for anecdotal checks only.
  - Legacy baseline at `aee6714`, the last legacy run (Apple M5 Pro, median ms per call; root-future / spawned-task):

    | Workload | Root ms | Spawned ms | Component bytes | Guest memory bytes |
    |---|---:|---:|---:|---:|
    | text | 0.018 | 0.015 | 20,468 | 65,536 |
    | io | 0.837 | 0.332 | 35,404 | 327,680 |
    | incoming-stream (4 MiB) | 24.124 | 17.279 | 21,155 | 65,536 |
    | incoming-stream-byob | 24.479 | 17.405 | 22,223 | 65,536 |
    | bounded-http-exact | 9.147 | 2.565 | 58,415 | 4,325,376 |
    | bounded-http-exact-byob | 0.425 | 0.140 | 59,550 | 4,259,840 |
    | unbounded-http-array-buffer | 2.138 | 1.913 | 54,594 | 8,519,680 |

    - Example components: `merge_docs` 114,105 bytes, `merge_task` 12,236, `template-task` 12,203.
    - The MIR pipeline today: `merge_task` 2,801 (`task-runner`) and 2,975 (`merge-task`); `template-task` 2,765.
    - Per 8 KiB stream chunk, legacy: 1 worker start, about 12 allocator entries, 2 promise records, 1 GC call.
  - *Done when:* there is one command and a committed baseline table.
- [ ] **P0.3 Legacy stopgaps (optional, hours).**
  - Memoize `infer_expr_type` with a per-function table.
  - Replace the per-backedge `collect-if-allocated` with the proportional budget trigger.
  - Only do this if the legacy backend must serve users during the migration.
- [x] **P0.4 Pin dependencies.**
  - Add `rev = "9e29f59…"` to `perry-parser` and `perry-hir` in `Cargo.toml`.
  - Add pliron as a pinned git dependency.
  - Extend the LLVM audit to reject `llvm-sys`.
  - *Status:* both revs are pinned, and pliron `6ff5b3c` is pinned in `crates/perry-mir`. The existing `audit_no_llvm` already rejects `llvm-sys` and `pliron-llvm` (`llvm-*`, `*-llvm`).

### P1. Frontend consolidation

- [ ] **P1.1 Type table.** Compute every expression's type once per function, keyed by HIR expression identity, and pass it to MIR construction. Delete the 228 recursive `infer_expr_type` calls with the legacy backend. *Done when:* the P0.1 compile-time contract passes.
  - *Status:* the compile-time contract passes.
    - Legacy: `infer_expr_type` keeps a per-query memo (`ssa/types.rs` `TypeMemo`), and depth 32 compiles in 0.4 s in a debug build. The memo cannot go stale, because the lowering environment cannot change during a `&self` query.
    - MIR: `crates/perry-lower` takes expression types from the MIR types of lowered operands, so it does not need a separate type table. The item stays open until the legacy backend is deleted.
- [ ] **P1.2 Merge validators into one subset checker that runs before MIR.**
  - Merge these passes, which run today in `mod.rs::compile_source`/`compile_resolved_hir`:
    - `text_contract` (AST and HIR);
    - `wit::validate_source`;
    - `source::validate_lowering`;
    - `objects::resolve_declared_types`;
    - `values::resolve_types`;
    - suspension validation.
  - Keep `prepare_command`, `resolve_bindings` and `initialization::extract` as HIR→HIR steps.
  - Add the R1–R7 diagnostics.
  - *Done when:* a single `frontend::check(&Hir) -> Result<TypedHir>` exists and each rejection has a contract.
- [ ] **P1.3 Rejection contracts.** Register a negative contract with a precise diagnostic for every R1–R7 form, under `ecma.*.rejected`.

### P2. MIR dialects in pliron

- [ ] **P2.1 Create `crates/perry-mir` with pliron dialects.**
  - `perry.core`: typed scalars (i32/i64/f64/bool), arithmetic with JS f64 semantics plus range-proved integer ops, compare, branch and cond-branch with block arguments, call and call_indirect, return, and throw/landing as explicit successors. No implicit unwinding.
  - `perry.heap`:
    - `alloc<shape>{tier}`, `field.get/set`, `array.new/get/set/len/push`;
    - `iface.pack/unpack`, `shape.test`;
    - `str.*` (UTF-8 views);
    - `dict.*`, `json.*`;
    - `bytes.view`.
  - `perry.async`: `async.call` (returns a promise), `await`, `promise.all/allSettled/race`, `settle`, `native.start(op)` producing a waitable, and `cancel`.
  - `perry.canon`: lift/lower of WIT values, `task.return`, and buffer ownership (`buffer.alloc{owner}`, `buffer.release`).
  - `perry.intrinsic`: the current capability operations (fetch, headers, streams, fs, timers, clocks, random, env, console, Date/Temporal, JSON, regex, TextDecoder/Encoder) as typed ops lowered later to runtime or helper calls.
  - Implement the pliron promotable interfaces on `alloc`, `field.get` and `field.set`, so that `mem2reg` handles non-escaping fields modelled as one promotable slot per field.
  - *Done when:* the dialects parse, print and verify, and round-trip tests pass.
  - *Status (in progress):*
    - `crates/perry-mir` provides these ops: `perry.binary`, `perry.compare`, `perry.br`, `perry.cond_br`, `perry.return`, `perry.alloc{tier}`, `perry.field_get` and `perry.field_set`.
    - It also provides the nominal, possibly recursive `perry.record` type and the `perry.tier`, `binary_kind` and `compare_kind` attributes. Constants use `builtin.constant`.
    - Verifiers and DCE side-effect interfaces are in place.
    - Added since: `perry.local`, `perry.load`, `perry.store` and `perry.undef`. The local ops implement the pliron promotable interfaces, and `mem2reg` turns them into block arguments.
    - Also added:
      - `perry.call` and `perry.async_call`, each verified against the callee's signature in the enclosing module;
      - `perry.await` and the `perry.promise` type;
      - `perry.array_new`, `perry.array_len`, `perry.array_get`, `perry.array_set` and `perry.array_push`;
      - `perry.string_const`, `perry.string_concat` and `perry.string_len`;
      - `perry.convert`.
    - R8 support: the `perry.optional` type; `perry.undefined`, `perry.wrap`, `perry.is_undefined`, `perry.to_number`, `perry.to_string` (pure) and `perry.unwrap` (throws `TypeError`); `MayThrowInterface` with a `perry.error_kind` (`Any`, `TypeError`, `RangeError`). `perry.array_get` takes an f64 or i32 index and returns `perry.optional<T>`.
    - Added typed buffer/view and JSON types, with verified intrinsics for their constructors, access and mutation. JSON-to-scalar and JSON-to-array conversions are explicit checked operations.
    - Added `perry.dictionary<T>` and verified construction/access/mutation/enumeration operations, including recursive type text round trips, optional element types, and required/optional named-field constraints. `Record<string, T>` and string index signatures lower to this type.
    - Remaining:
      - `iface.*`, including non-JSON views between dictionary schemas; checked JSON record/array/dictionary views are implemented;
      - `perry.canon` and the remaining intrinsic signatures. `perry.intrinsic` is now one verified op with a signature table for console, environment/arguments and clocks.
      - a record-field promotion story for SROA. `mem2reg` promotes whole slots, so record fields need their own pass (P3.4).
    - pliron constraint: attribute keys are global, so ops use `perry_*` attribute names.
- [ ] **P2.2 → HIR→MIR builder.**
  - One structural lowering in `src/frontend/to_mir.rs`, driven by the type table. It makes no representation decisions: tiers and layouts are left to passes.
  - The `await` continuation blocks, root tracking and per-type special cases in `ssa.rs` disappear.
  - *Done when:* every conformance-registry source builds into verified MIR. Codegen is not required yet.
  - *Status (first slice):* the builder is `crates/perry-lower` (`lower_source`, `lower_module`), a separate crate rather than `src/frontend/to_mir.rs`.
    - Every local is a `perry.local` slot, and pliron `mem2reg` then builds the SSA joins and loop-carried block arguments, so the builder hand-rolls no SSA.
    - Supported:
      - synchronous functions over number, boolean and string;
      - locals, `=`, `++`/`--`;
      - arithmetic, comparisons, `&&`/`||` (short-circuit through slots), `!`, unary `-`/`+`, `?:`;
      - `if`, `while`, `do`/`while`, `for`, `break`/`continue`, `return`;
      - direct calls, string `+` and `.length`.
    - Everything else is rejected with a diagnostic.
    - Since added: records (interfaces and literals unify by sorted field names), arrays, `push`, index and property reads and writes, R8 optionals with ECMAScript coercions and `=== undefined`. Eight tests pass.
    - Since added: async functions, `await`, and `setTimeout` from `timers/promises` / `node:timers/promises` (`perry.timer`).
    - Since added: Error/TypeError/RangeError values, primitive throws (number, boolean, string), `try`/`catch`/`finally`, Error identity and `instanceof`, `.message`, and string strict equality. Finalizers preserve or override return, throw, break and continue, including across `await`.
    - CLI worlds lower module statements to an async command entry; non-command worlds diagnose unsupported module initialization instead of silently dropping it.
    - Next: globals and module state, truthiness, ordered string comparisons, labeled control flow, other Error subclasses/cause and remaining thrown-value types.
- [ ] **P2.3 MIR snapshot tests** replace the 10 `waffle_ir` references in the test suite.

### P3. Pass pipeline

Reused from pliron:

- [ ] **P3.1** Configure SCCP, DCE, `simplify_cfg` and `mem2reg` in the pass manager.
  - *Status:* `perry_mir::pipeline::canonicalize` runs mem2reg, then DCE and `simplify_cfg` to a fixpoint, verifying before and after. SCCP still needs constant-folding interfaces on the `perry.*` ops.

Owned (each with unit tests on MIR snippets):

- [ ] **P3.2 Inliner.** Bottom-up, with size budgets. Always inline R5 arrow arguments and runtime accessors.
  - The JSON-only callback inliner was removed with the accepted reviver/replacer exclusions. Implement shared inlining for supported array/promise callbacks when those builtins migrate; do not restore callback machinery solely for JSON.
- [ ] **P3.3 Escape summaries.** Per function parameter, computed to an optimistic fixpoint (Zena `escape.zena`). Store, return, throw, capture into module state, passing to an escaping parameter, identity comparison and import arguments all count as escapes.
- [ ] **P3.4 SROA.** Non-escaping `alloc` dissolves into SSA values and block parameters (Zena `sroa.zena`).
- [ ] **P3.5 Integer ranges.** Loop counters, indices and lengths become i32/i64 where proven. Otherwise use f64.
  - Where `0 <= i < length` is proven, rewrite `perry.array_get` to an unchecked read, drop `perry.optional` and fold the `perry.unwrap`/`perry.to_number` uses (R8).
  - Use `perry.is_undefined` guards as range facts on the defined branch.
- [ ] **P3.5b Throw edges.** Ops implementing `MayThrowInterface` (`perry.call`, `perry.await`, `perry.unwrap`, `perry.array_set`) branch to the nearest handler or the function's exceptional exit.
  - Implemented: the builder emits an adjacent `perry.caught_status` and explicit CFG branch inside handlers. The pair is verified and preserved by canonicalization; codegen joins normal results with the status at a landing. Unhandled failures use the status return convention. A pending exception belongs to the current task; catch consumes it and promise rejection retains the original value, so repeated awaits and rethrows preserve identity.
  - Implemented: structured finalizer completion and removal of unreachable handlers before SSA construction. Regressions cover nested catches/finalizers, implicit runtime failures, primitive equality including NaN, repeated rejected awaits, host suspension and overlapping tasks. `ecma.error.control_flow` is ratcheted against Node, with directed completion witnesses and generated Unicode messages/repeated waits; the wider `ecma.error.values` contract remains pending on causes/combinators/codecs.
  - Remaining: integrate check elimination with P3.5, describe the pair's effect/dependency in future scheduling passes, and remove redundant checks without changing exception order.
- [ ] **P3.6 Shapes and interface views.**
  - Exact layouts for closed types: references 4 bytes, f64/i64 8-aligned, packed booleans, optional-presence bits.
  - Per-(shape, interface) field-offset tables.
  - Union discrimination by shape id.
  - Layout metadata checkpoint: native records carry a compiler-emitted type-descriptor address and a per-allocation property-order table before their eight-byte slots. Recursive descriptors include field names/types and array element types. MIR verifies property-order permutations and JSON-compatible type graphs. This preserves creation order independently of canonical sorted shapes; packed layouts and general structural interface views are still pending. JSON-backed typed references now distinguish checked views from aligned native storage with a low reference bit; native fast paths retain direct field/element loads and stores. Codegen omits view branches/runtime calls entirely when the module cannot create typed views. P3.5/P3.6 must remove redundant view branches and exception effects when native provenance is proven, and avoid repeated schema validation where alias analysis permits it.
- [ ] **P3.7 Tier classification.** Implements D3, including the loop-growth rule. Emit a per-site report: `perry-wit --explain-tiers`.
- [x] **P3.8 → Async split.** `perry_codegen::async_split`.
  - Each async function becomes a ramp (allocate the frame in `task`, spill arguments, run eagerly until an await actually suspends) and a `step` state machine.
  - Frame fields are the values live across each `await`, by backward liveness over the MIR CFG. They are stored where defined and loaded where used, so resume entries need no SSA repair.
  - Uncontended settled awaits continue inline without queue allocation. Pending promises and competing ready work use the task queue, which drains before the next host turn.
  - Resuming inside a loop enters the loop at a second block; `pliron_wasm::reducible` inserts a guard block (LLVM `FixIrreducible`), so the emitter's structuring still applies.
  - An exception in an async function rejects its promise with the original exception value; `await` restores that value and routes its status through the local handler or exceptional exit.
  - Tests: `async_functions_await_each_other` (raw, no host) and `tests/async_component_test.rs` (WASI P3 timers, overlapping calls, no memory growth across tasks).
  - Open: combinators (`Promise.all` and friends) with one waiter node per operand.
- [ ] **P3.9 → Root planning.**
  - Interprocedural may-collect summary.
  - Spill sets at may-collect call sites are the `instance` references in the liveness set.
  - *Done when:* tests prove there are no spills in allocation-free loops.
- [ ] **P3.10 Canon lowering.** Lower `perry.canon` to buffer-tier allocations with explicit owners, replacing the schema-guided GC tracing of canonical layouts. Port the layout logic from `wit/adapter*.rs`. WIT `option` values and direct optional TypeScript values at WIT scalar boundaries remain unmigrated; optional record properties do not complete that ABI work.

### P4. Wasm emission without WAFFLE

- [x] **P4.1 `wasm` dialect** close to core instructions: values stay SSA, with block arguments.
  - *Status:* `crates/pliron-wasm` is kept free of perry code so it can be published later.
  - Ops: `wasm.num` (an opcode attribute carrying its signature and effect class, covering i32, i64, f64 and conversions), `wasm.select`, `wasm.load`/`wasm.store`, `wasm.memory_size`/`wasm.memory_grow`, `wasm.call`, `wasm.import_func`, and `wasm.br`/`wasm.cond_br`/`wasm.return`/`wasm.unreachable`.
  - Constants and functions are pliron builtins. Exports are a function attribute.
  - Lowering from `perry.*` is a direct translation in `crates/perry-codegen`, not pliron dialect conversion. The translation splits blocks for throws and array growth, which the conversion framework does not model well.
- [ ] **P4.2 → Emitter** (`pliron_wasm::emit`, replacing the planned `crates/perry-emit`):
  - [x] verify reducibility;
  - [x] dominator-tree structuring (Ramsey, "Beyond Relooper");
  - [x] tree folding of single-use values that keeps effects and traps in order;
  - [x] rematerialized constants;
  - [x] cyclic block-argument moves through the operand stack, so no temporaries are needed;
  - [x] type-grouped local declarations and an optional name section;
  - [x] output with `wasm-encoder` 0.261;
  - [ ] local coalescing by liveness (pliron `analyses::liveness`);
  - [ ] peephole: drop a `br` to the end of the directly enclosing `block` when it is in tail position. Ramsey emits `if … br 1 else br 1 end` for diamonds whose arms only join;
  - [ ] `local.tee` for a value that is used once in the next statement and again later;
  - [ ] `br_table` for switch-shaped CFGs.
  - *Done when:* differential tests against the legacy output for scalar, control flow and exception fixtures pass, and the output is no larger than WAFFLE's on those fixtures.
  - *Status:* 8 tests validate with wasmparser and execute in wasmtime: diamonds, loops, nested loops with break/continue, parameter swaps, recursion, memory, imports, effect ordering, and rejection of irreducible CFGs. The differential comparison against WAFFLE waits for P6.1.
- [x] **P4.3 Keep and generalize `link.rs`.** Now `src/link`.
  - The root `build.rs` builds `perry-runtime` as one more PIC helper library (`runtime.wasm`).
  - Codegen imports `perry_*` functions from `__perry_helper`, and the linker places their data after the module's static data.
  - The linked module has no imports besides the WIT world's.
  - `wasm-opt -Oz` stays optional and is not wired in.
  - Follow-up for the P4/P7 size gate: prune unused helper data as well as code. The native-JSON checkpoint produced a 27,209-byte stripped template because runtime data is retained wholesale; the serialization-controls checkpoint measures 27,863 bytes for the template and 27,899 bytes for `merge_task`. The reviver checkpoint measures 27,911 and 27,947 bytes respectively; the function-replacer checkpoint measures 27,903 and 27,939 bytes (CLI: 33,408 bytes). The constrained-dictionary checkpoint measures 28,087 and 28,123 bytes (CLI: 33,600 bytes). The optional-record checkpoint measures 28,127 and 28,163 bytes (CLI: 33,640 bytes). Template code remains 2,638 bytes; its data section grows from 25,150 to 25,190 bytes, accounting for the entire 40-byte increase. The enumeration checkpoint measures 28,326 bytes for the template, 28,362 for `merge_task` and 33,831 for the CLI. Template code is 2,653 bytes and data is 25,374: the 199-byte increase is 15 bytes of code and 184 bytes of data. These features must not conceal that existing cost. Track code and data sizes separately, then restore the small-component baseline. The accepted-profile/string checkpoint measures 36,450 bytes for the template, 36,486 for `merge_task` and 41,995 for the CLI. Template code is 2,746 bytes (+93), and data is 33,398 (+8,024), for a total increase of 8,124 bytes. Restored Unicode casing brings property tables into the runtime; unused runtime data is still retained. This is a size regression despite smaller JSON nodes and allocation-free uncontended settled awaits; the small-component gate remains open. The text-codec checkpoint measures 36,830 bytes for the template, 36,866 for `merge_task` and 42,371 for the CLI. Template code is 2,854 bytes (+108) and data is 33,670 (+272): another 380 bytes in a component that does not use codecs. Unused runtime data and shared-code growth remain explicit P4/P7 work. The filesystem-metadata checkpoint measures 36,938 bytes for the template, 36,974 for `merge_task` and 42,498 for the CLI. Template code is 2,953 bytes (+99) and data is 33,678 (+8), a total increase of 108 bytes. Shared task cleanup now releases explicitly claimed import buffers; the unused-data and small-component gates remain open.
- [x] **P4.4 Remove `waffle`.** Done 2026-10-09 together with the whole legacy backend. `cargo tree -i waffle` is empty.

### P5. Runtime as Rust (`crates/perry-runtime`, `no_std`, compiled like the helpers)

- [ ] **P5.1 Buffer allocator.** `talc` (decided in D3) behind `cabi_realloc`. Owner records for call, transfer and stream. *Done when:* canonical return areas and stream buffers are released at owner end, which the tests check.
  - Canonical import buffers now have independent allocation headers and explicit release/transfer to a task. Filesystem preopen lists and returned optional strings release at consumption; CLI environment/argv release with their owning task. General transfer and stream ownership remains required.
- [ ] **P5.2 Regions.**
  - Frame stack: a bump stack per task.
  - Task regions: chunk lists with a per-region list of objects that hold references.
  - Bulk free at `task.return` / `task.cancel`.
  - *Status:* `crates/perry-runtime` (4.8 KB on wasm32 with talc and `fmod`) provides:
    - task regions: a 64 KiB chunk chain with a free list, dedicated chunks above 16 KiB, and nested `perry_task_begin`/`_end` marks;
    - dense arrays (`perry_array_new`/`_grow`, layout in `array.rs`);
    - `perry_f64_rem`.
  - It owns memory, exports it, and links with a 64 KiB stack. `crates/perry-codegen/build.rs` builds and embeds it the way the root `build.rs` builds the helpers.
  - Still open: the frame stack and the per-region list of reference-holding objects. That list matters only once `instance` objects exist (P5.3).
- [ ] **P5.3 `instance` heap.** Size-class pages, large objects, side mark bitmaps, explicit mark stack, lazy sweep, proportional trigger, shape pointer-map tracing.
  - *Done when:* the P0.1 scaling contracts pass on the MIR backend.
  - *Status of that criterion:* already met at core-module level without the collector. Since the builder cannot express module state (R4), every allocation is `task` tier. `perry-codegen`'s `scaling_contracts_are_linear` measures a fuel ratio for 4n vs n of 3.99 for `retained_records`, 3.98 for `number_push` and 4.01 for `temporary_records` (both of the first two are known failures on the legacy backend), at 130–200 fuel per item. The collector is needed once module state lands.
  - A cycle-collection contract passes.
  - Allocator entries are 0 per iteration for the temporary-record loop.
- [ ] **P5.4 Scheduler.**
  - *Status:* `perry_runtime::region::Task` holds the run queue, the pending host subtasks (handle → promise and value) and the waitable set; `perry_runtime::promise` queues waiters in order. `[async-lift]<export>` sets `context.set 0` to the task, and `finish` drives ready frames, then returns `EXIT` after `task.return` or `WAIT` on the task's set. The callback settles returned subtasks. YIELD is never returned.
  - Successful async completion drains host subtasks even after the export promise settles. Every await observes its promise; uncontended settled awaits continue inline. After ready continuations drain, an unobserved rejection fails the task immediately, cancelling native work before releasing its region. Commands return an error; exports without an error result trap. Tests distinguish handlers installed in the same turn from handlers deferred until a later timer and verify cancellation of hour-long timers on failure. Checked rejection entries are discarded to avoid repeated scans.
  - `ecma.promise.await_ordering` is exempt: uncontended settled awaits continue inline. Raw/component tests assert the accepted trace and zero memory growth over 100,000 repeated settled awaits; pending waiters retain registration order. Promise combinators, `.then` callbacks and non-promise awaits still need migration under this profile.
  - Cancellation now fans out to timers and output with baseline sync `subtask.cancel`, `stream.cancel-write` and `future.cancel-read`. Waitables detach before cancellation; native handles close before task memory is freed. Callback cancellation acknowledges `task.cancel`; immediate process exit shares the cleanup. Real nested-component tests cancel timers, blocked stdout/stderr writes and pending completion futures, verify queued lines are abandoned, and repeat calls without memory growth or retained host operations. General stream/read cancellation follows when those features migrate.
  - Synchronous exports now drain detached ready continuations before lifting their results and reject unhandled rejections. Pending native work cannot outlive a synchronous call (R3): cleanup is attempted before freeing the region. Wasmtime rejects blocking cancellation on that call stack with `CannotBlockSyncTask`; the task's buffers remain live in the poisoned store until disposal. Tests cover inline settled-call order, memory reuse, raw-export rejection, and native-future disposal on both return and exception paths. Use async WIT exports for work that must wait on the host.
  - Native filesystem metadata and directory operations own their borrowed preopen descriptor and preallocated canonical result area until completion or acknowledged cancellation. Import payloads use `cabi_import_realloc`: Wasmtime invokes it with an empty context slot, so it allocates independent buffers instead of guessing a task. Consumers release temporary strings/lists, while CLI environment and argv explicitly claim their buffers into the current task. Pending large `other` variants, overlapping exports and repeated component-caller cancellation have memory/resource regression coverage. File and general stream transfers remain open.
  - Run queue of frames, using an intrusive list.
  - Waiter table mapping waitable handle to frame and state.
  - Callback dispatch and the export entry, which sets `context.set 0` to the task.
  - Never return YIELD except under a fairness budget.
  - Cancellation fan-out with sync `subtask.cancel` and sync `stream.cancel-read`/`-write`.
- [ ] **P5.5 Port the remaining `.wat` runtime** (about 1.5k lines: utf8, decoder, json, errors, filesystem, http handler, random, regex search) to Rust or to MIR builders. Delete `runtime/builder.rs`, the fixed-address scheme (addresses 0–152) and every `const OFFSET` layout. Layouts come from P3.6 or from Rust structs.

### P6. Feature migration onto the MIR pipeline (the only pipeline)

- [x] **P6.1 Component boundary and pipeline cutover.** Decided 2026-10-09: no `--backend` switch; the legacy backend is deleted (see Status). Unsupported programs fail with a diagnostic.
  - `src/compiler` runs TS → HIR → MIR → canonicalize → codegen → link → component. `compile_core` produces raw exports for tests.
  - `src/compiler/bind.rs` binds WIT exports to exported TypeScript functions by SDK implementation name. Extra TypeScript exports stay internal.
  - `perry_codegen::boundary` emits canonical-ABI glue for `bool`, integers, `f32`, `f64`, `string`, `list<T>` and records: flat or memory lifting, and lowering into a return area. It also emits `cabi_realloc`, plus `cabi_post_<export>`, which ends the task.
  - Arguments are lowered into a task the host's `cabi_realloc` opens and the export adopts, so arguments and results live in the task region and are released in bulk.
  - An exception escaping an export without an error type traps. `result<T, E>` exports map the status later (P6.2).
  - Strings (accepted profile): UTF-8 with a cached scalar count, passed through the boundary without transcoding.
    - Constants are static data.
    - Concatenation appends in place to a shared buffer, so the `string_append` contract passes.
    - Lone surrogates and WTF-8 are excluded.
  - Conformance: `crates/conformance/passing.txt` is a ratchet. Listed contracts must pass. An unlisted contract that passes fails the campaign until it is added. Unlisted failures within the profile are `pending` and are not shrunk; documented deviations are `exempt`.
  - Tests:
    - `tests/component_test.rs` passes WIT values through wasmtime's component model, and checks that memory does not grow across calls.
    - `tests/mir_codegen_test.rs` runs linked core modules with fuel.
  - Builder fixes found on the way:
    - Records with equal field names but different field types get suffixed names (`shape_c_v_2`).
    - Recursive interfaces reserve a name.
    - Named types are memoized.
    - Object literals take an expected record type.
  - Smaller MIR follow-ups found while emitting:
    - remove a `perry.unwrap` dominated by another unwrap of the same value (P3.5b). `point.x; point.y` currently checks twice;
    - inline `fmod` for proven integers (P3.5). `perry_f64_rem` is the largest per-iteration cost in the scaling loops.
- [ ] **P6.2 Migrate in registry order.** Each group is done when its supported-profile contracts and matching `tests/*` suites pass on MIR; accepted deviations are not migration obligations:
  1. `ecma.number`, `ecma.math`, `ecma.string`
  2. `ecma.object`, `ecma.array`
  3. `ecma.json`, `ecma.date`
  4. `ecma.promise`, `node.timers.promises.*`
  5. `web.console`, `web.crypto`, `web.performance`, `web.textencoder`
  6. `node.process`
  7. `node.fs.*`
  8. streams and BYOB
  9. Fetch, then the HTTP handler
  10. CLI command
  - CLI/exception checkpoint: `wasi:cli/run` executes top-level statements and awaits; normal completion returns `ok`, uncaught throws return `err`. `tests/command_component_test.rs` covers both before and after suspension. This is not yet the complete CLI migration.
  - CLI I/O checkpoint: the linker forwards reachable runtime host imports, deduplicates compatible imports and rejects conflicting signatures. Console stdout/stderr use per-task P3 stream queues and completion futures; partial writes, output failures, interleaved tasks and region reuse are exercised with real Wasmtime hosts. Environment lookup/keys, host arguments, monotonic time and Date.now are restored. The ratchet now includes `web.console.routing`, `web.performance.monotonic` and `node.process.environment`. `examples/cli.ts` is built and executed by the Nix checks. Console currently accepts one string argument and requires async WIT exports.
  - CLI lifecycle checkpoint: numeric `process.exitCode` supports validated assignment, reading and clearing; graceful status drains pending work and output. Immediate `process.exit` cancels current native work and bypasses catch/finally, with modulo-256 status and an unset default of zero. Explicit SDK command exports run after top-level initialization and adapt their boolean `ok` result, including async implementations. Tests exercise exact exit codes, cancellation, result/error precedence and invalid command signatures. The ratchet now includes `node.process.exit_status`.
  - Finish CLI: remaining CLI contracts and console formatting. String exit codes and optional-valued exit-code expressions still need migration. Promise combinators and callbacks must preserve the accepted inline/pending scheduling profile, without imposing synthetic microtask FIFO ordering. Do not mark group 10 complete until those contracts pass.
  - Uint8Array checkpoint: distinct MIR buffer/view types; zero-filled ArrayBuffer and length constructors, copying construction/from/of, indexed reads/writes, length/byteLength/byteOffset/buffer, shared subarray, copied slice, and overlap-safe set from bytes or numeric arrays. Range errors are catchable and set validates before mutation. WIT list<u8> accepts/returns Uint8Array, adopting incoming task storage and returning view spans without element copies. Component tests cover aliasing, independent copies, suspension, overlapping tasks and stable memory across repeated 50 KB calls. `ecma.uint8array.set` and `ecma.uint8array.views` compare with Node over generated byte sources and IEEE-754 boundaries.
  - JSON checkpoint: `perry.json` is an explicit six-variant tree, backed by task-owned runtime values and a pure codec shared with the existing helper. Parse/stringify preserve numeric and insertion-key ordering, duplicate-key semantics and UTF-8 text. Lookups, existing-field mutation, dense-array writes, strict equality, `typeof` and `Array.isArray` preserve composite aliases. Checked scalar conversions yield native number/boolean/string values; checked `JsonValue[]` conversion shares the existing dense storage. Fresh literals can construct JSON trees without `any`. Syntax errors, invalid conversions and cyclic serialization are catchable. Component tests cover aliases across suspension, overlapping exports and stable memory across repeated large documents. `ecma.json.values` compares exact output with Node over directed boundaries and generated JSON documents.
  - Native JSON checkpoint: `perry.json_pack` creates views over stored native records and arrays without traversing or copying them. Serialization walks native slots through type descriptors and retains each record's property creation order, including WIT record inputs. JSON lookups and checked writes share native storage, observe array growth and preserve aggregate identity; incompatible writes raise TypeError before mutation. Recursive shapes, cyclic serialization, overlapping async calls and native optional-value serialization have regression coverage. Native optional fields serialize by omission and optional array elements as null; root undefined remains undefined. `ecma.json.native` compares native/JSON alias graphs and creation order with Node. Its recursive-record-alias case exposed a compiler stack overflow; named alias resolution now shares the interface reservation mechanism, with the failure retained in lowerer and conformance regressions.
  - Parsed typed-view checkpoint: `perry.json_unpack` validates record/array schemas and retains the original storage. Repeated views compare by source identity; packing unwraps them, preserving extra fields and creation order. Native aggregate slots accept compatible parsed aggregates. Typed reads recheck values after JSON-alias mutation and remain effectful even when unused; incompatible reads/writes raise catchable TypeError. Recursive validation memoizes shape/storage pairs and bounds depth to 128. WIT result lowering, byte constructors, array growth and async frames understand these views. Component regressions cover invalid schemas, stale reads, cyclic aliases, result-boundary failures, suspension and overlapping tasks. `ecma.json.typed` supplies generated valid alias cases for comparison with Node. Adding throwing reads/writes also exposed a finalizer block-order defect in the existing exception contract: codegen now visits reachable blocks in reverse postorder so dominating definitions precede their uses, with a loop-finalizer regression.
  - JSON dictionary checkpoint: parsed objects have stable task-owned headers, ordered entries and hash buckets. Insertion, deletion, compaction and growth retain typed/JSON aliases; overwrite retains order, while delete/reinsert moves a string key to the end. `Object.keys` returns independent ordered snapshots and `Object.hasOwn` distinguishes missing properties from present undefined values, including native optional slots. Nested literal and assigned undefined values serialize by omission in objects and as null in arrays. Native records stay closed; deleting an existing dense-array element raises RangeError. Assigning an absent `__proto__` property rejects the unsupported prototype setter; existing parsed data properties remain writable. Native `JsonValue[]` keeps its type descriptor, while parsed-array narrowing checks every element and uses typed views, preventing undefined from bypassing native element checks. Runtime and component tests cover growth/compaction, stale reads after deletion, reinsertion through typed views, numeric-key boundaries, duplicate keys, Unicode, aliases and key snapshots across suspension/overlapping exports, and stable memory after task reclamation. `ecma.json.dictionary` compares dictionary mutations and enumeration against Node over directed and generated key sequences.
  - Typed dictionary checkpoint: `Record<string, T>` and pure string index-signature interfaces/type literals use ordered native storage with unboxed scalar/reference slots. Missing reads are optional; present optional values remain own properties. Get/set, delete, keys, values and has-own preserve order and identity through growth, recursive types and checked JSON/record views. Native records exposed as dictionary views keep their closed shapes. Byte-view and array values need no JSON conversion for ordinary dictionary operations. Frontend validation rejects mixed or non-string index signatures before the pinned HIR can drop their constraints. Component tests cover stale JSON aliases, optional values, recursive schemas/cycles, signed zero, snapshots, suspension, overlapping exports and task-memory reuse. A host-NaN regression exposed a sentinel collision; numeric-to-optional promotion, numeric collection reads and JSON narrowing now canonicalize present NaNs without changing signed zero. `ecma.object.dictionary` compares valid typed dictionary programs against Node.
  - Constrained dictionary checkpoint: string index signatures can mix with required and optional named fields, including JSON-compatible refinements and recursive schemas. MIR construction retains field names and source evaluation order; descriptors retain named types and presence constraints while native entries keep uniform unboxed slots. Required optional-valued fields must exist, distinct from optional absent fields. Native writes and deletes enforce original constraints through broader dictionary/JSON aliases; parsed views recheck current field types and required presence. Frontend normalization preserves named fields on either side of an index signature, including inline and nested annotations. Component tests cover invalidating aliases, recursive views, required-field protection, byte-view fields, numeric keys, signed zero, construction effects, growth/compaction, suspension, overlapping calls and memory reuse after large documents. `ecma.object.constrained_dictionary` compares valid fixed programs with generated inputs against Node and joins the ratchet. Non-JSON schema adaptation remains part of R2; methods/accessors, inheritance and multiple/non-string signatures remain unsupported. Validation: workspace tests, strict host/Wasm Clippy, formatting and local macOS Nix checks pass. The full seed-0 campaign (8 cases, depth 3) has 22 passing and 40 pending contracts, 176 generated cases and 291 executions, with unchanged sources during execution.
  - Optional record checkpoint: interfaces, aliases and contextual literals retain optional presence separately from optional-valued required fields. Native fields remain unboxed in fixed slots; records with optional fields own a bounded property-order table, while required-only records keep static tables. Typed/JSON writes can insert absent declared fields, deletion clears optional slots, and overwrite/reinsertion preserve ECMAScript order. Parsed views check both required presence and current field types; broader views cannot bypass native required-field or closed-shape constraints. Record keys/has-own need no JSON conversion, including byte-view fields. Construction and WIT input lifting share one allocation emitter. Tests cover verifier round trips/rejections, recursive schemas, initially empty records, independent order tables, undefined presence, signed zero, invalidating aliases, snapshots, compatible narrowing at WIT results, suspension/overlapping exports and memory reuse after large inputs. `ecma.object.optional_record` compares fixed programs with generated values against Node and joins the ratchet. WIT option values and direct optional scalar boundary adaptation remain separate P3.10 work. Validation: workspace tests, strict host/Wasm Clippy, formatting and local macOS Nix checks pass. The full seed-0 campaign (8 cases, depth 3) has 23 passing and 40 pending contracts, 184 generated cases and 301 executions, with unchanged sources during execution.
  - Enumeration checkpoint: `perry.enumerate` handles keys, values and entries over native and parsed storage, preserving ECMAScript order, optional values, scalar snapshots and aggregate identity. Fixed-position typed tuples back entry pairs and explicit tuple declarations, including byte-valued entries. Homogeneous native values remain unboxed; heterogeneous JSON-compatible fields use checked JSON slots. Typed parsed views recheck values, including extra properties, and tuple narrowing validates arity and positions. Fresh literal inference is recursive and preserves assignment semantics through a separate JSON path. Tests cover native and parsed aliases, incompatible writes, stale reads, arbitrary/numeric keys, nested mixed literals, BMP strings, signed zero/NaN definedness, task-memory reuse and snapshots across suspension/overlapping exports. `ecma.object.enumeration` adds fixed-program generated-input comparisons; it is a P6.5 seed. Native tuple appends, general tuple methods, recursive tuple aliases, non-JSON heterogeneous unions and WIT tuples remain pending. The accepted-profile checkpoint below replaces the former astral-string restriction with whole-scalar enumeration. Validation: workspace tests, strict host/Wasm Clippy, formatting and local macOS Nix checks pass. The focused seed-0 campaign passed both directed cases and 16 generated cases; the full campaign (8 cases, depth 3) has 24 passing and 40 pending contracts, 192 generated cases and 311 executions, with unchanged sources during execution.
  - String checkpoint: typed intrinsics restore indexing, charAt/at, codePointAt, slicing, search, Unicode casing, trim variants, repeat, string split and concat over native and parsed strings. UTF-8 spans and cached scalar counts replace UTF-16 boundary machinery. Astral characters stay whole. Inferred bindings preserve optional results while explicit schemas remain checked. Component tests cover argument effects, numeric edges, astral text, immutable aliases, suspension, task reuse and repeat errors. `ecma.string.utf8` uses BMP-only Node comparisons and partition/rejoin, trim/case, repeat and split relations. String.fromCodePoint, padding, replacement and bounded literal DFA regexes remain migration work; charCodeAt/fromCharCode and dynamic RegExp are rejected.
  - Accepted-profile simplification (2026-10-09): delete JSON callback traversal, original-source tracking, property-list filtering and indentation. Codec nodes shrink from 40 to 32 bytes (20% less node storage). Keep compact JSON ordering, duplicate keys, cycle/depth errors and checked native/parsed aliases. `ecma.json.roundtrip` and `ecma.string.operations` passed their full registered domains and enter the focused ratchet. Exemptions retain their historical specifications and generators without becoming migration obligations.
  - Accepted-profile validation: full workspace tests, strict host/Wasm Clippy, formatting and all eight local macOS Nix checks pass. The final frozen-source seed-0 campaign (8 cases, depth 3; `target/conformance/run-5SGrB2/report.json`) reports 24 passed, 38 pending and 4 exempt contracts, with 192 generated cases, 314 executions and `source_unchanged: true`. The new rejection contract covers JSON controls, code-unit APIs (including folded zero-argument forms) and dynamic RegExp construction. Core JSON, string and dictionary contracts remain ratcheted. Component tests separately cover astral scalar indices/enumeration, 127 nested JSON objects, suspension/overlap and task reuse. A 100,000-iteration repeated settled-await test confirms inline ordering and no memory growth. Emitted-size growth is recorded in P4.3 above; neither the refactor nor P6.5 is complete.
  - Text codec checkpoint: TextEncoder.encode and strict incremental TextDecoder now use native byte views and distinct task-owned MIR references. UTF-8 labels, fatal-by-default behavior, BOM handling, partial characters, empty flushes and retained unread input after streaming errors preserve the earlier profile. Replacement decoding remains rejected. The source adapter preserves receivers, typed declarations, argument order and duplicate/unknown option effects before HIR lowering; options require literal plain data properties and typed booleans. Component tests compare 1,620 valid/malformed chunk-and-BOM cases with Node, separately check the retained-input spec difference, and cover mutation safety, aliases, optional flags, suspension, overlapping exports and task reuse. These are fixed-program seed cases, not P6.5 completion. `web.textencoder.values` and `web.textdecoder.streaming` join the passing ratchet. The broader `web.textencoder.subviews` expression contract remains pending on generated Math/Date dependencies; it stays intact alongside a focused byte-content/identity contract.
  - Text-codec validation: workspace tests, strict host/Wasm Clippy, formatting and local macOS Nix checks pass. The frozen-source seed-0 campaign (8 cases, depth 3; `target/conformance/run-BetwJ2/report.json`) reports 26 passed, 37 pending and 4 exempt contracts, with 208 generated cases, 351 executions and `source_unchanged: true`. Separate focused campaigns exercise 32 generated cases per codec contract. Size growth is recorded in P4.3; the broader refactor and P6.5 remain incomplete.
  - Filesystem metadata checkpoint: `fs/promises` and `node:fs/promises` support named, namespace and default imports of stat, mkdir, unlink and rmdir. Typed Stats preserve size, mtimeMs and file/directory predicates through helpers and records. Longest-preopen selection, lexical path normalization, protected mount roots, read-only capabilities and symlink confinement preserve the earlier numeric WASI error profile. Literal options preserve duplicate/unknown argument effects; bigint, throwIfNoEntry:false and recursive operations reject with invalid (12). File read/write, directory listing, transfer cancellation and their full legacy option surface remain pending; this does not exempt or replace `node.fs.directories` or the broader filesystem contracts. `node.fs.metadata_lifecycle` adds a focused nested-directory differential seed. Component tests cover delayed native results containing large optional strings, negative/absent timestamps, overlapping exports, caller cancellation and stable memory/resource reuse.
  - Filesystem validation: workspace tests, strict host/Wasm Clippy, formatting and all eight local macOS Nix checks pass. The frozen-source seed-0 campaign (8 cases, depth 3; `target/conformance/run-ObA9VU/report.json`) reports 27 passed, 37 pending and 4 exempt contracts, with 216 generated cases, 361 executions and `source_unchanged: true`. The focused lifecycle campaign (`target/conformance/run-Um8kIR/report.json`) passed both directed cases and 32 generated cases. Six component tests cover metadata/mutations, delayed canonical results, caller cancellation, confinement/options, empty/relative preopens and required async WIT imports. The import allocator additionally checks grow/shrink/empty release, and existing CLI/environment and output/timer cancellation tests pass after sharing the import allocator. P6.5 and the broader refactor remain incomplete.
  - Next byte consumers: finish the remaining byte methods and filesystem, then streams and fetch consumers. Resizable/detached/shared buffers, mapped factories and other typed arrays remain outside the Uint8Array checkpoint. Decode currently accepts Uint8Array views; encodeInto, replacement decoding, non-UTF-8 labels and arbitrary option objects are outside the codec profile. The broader `ecma.uint8array` contract still needs its remaining byte methods.
- [x] **P6.3 Strip thread flags everywhere.** The legacy tests, fixtures and scripts are gone, and README and conformance no longer pass the flags. Async (P3.8) must keep it that way. *Done when:* `rg -n 'thread-|stackful|threading|more-async-builtins'` returns nothing outside history and this file.
- [ ] **P6.4 Portability check.** Emitted components validate with wasmparser **default** features, so no 🧵, 🚟 or 🚝. Optionally smoke-run `merge_task` and a CLI example in jco.

- [ ] **P6.5 Generated-program search and metamorphic correctness campaigns.** Restore the breadth/depth of the earlier tests/conformance generators in a form aligned with the typed MIR subset. This is a separate required work item, not satisfied by directed witnesses or random function arguments.
  - Inventory the generators, shrinkers, saved failures and metamorphic relations in pre-refactor history. Reuse useful machinery with explicit supported-feature profiles; preserve directed regressions alongside it.
  - Generate well-typed, bounded whole programs: declarations, calls, branches, loops, records/arrays and aliasing; then exceptions/finalizers, async dependencies/combinators, module state and host operations as each lands. Control expression depth, call depth, loop counts, allocation churn and interleavings independently. Unsupported capabilities must not stop exploration of supported combinations.
  - Differentially execute the agreed semantic domain against Node and compare values, exception identity/class/message, observable effect order and completion. Use explicit oracles for intentional subset differences; do not normalize away NaN/-0, Unicode, dense-array or scheduling discrepancies silently.
  - Apply semantics-preserving transformations with stated preconditions: alpha-renaming, pure let introduction, equivalent branch/loop forms, inlining eligible pure calls, aggregate scalarization, catch/rethrow and non-overriding finally insertion. Compare original/transformed programs and, when optimization controls exist, optimized/unoptimized compilation.
  - Generate byte-buffer alias graphs with nested subviews, independent copies, overlapping set in both directions, omitted versus NaN endpoints, invalid writes/ranges and byte coercion. Preserve alias preconditions during shrinking and transformations; carry views across await and WIT boundaries, and compare both bytes and storage identity. The directed Uint8Array contracts are seed cases, not completion of this search work item.
  - Extend codec search from `web.textencoder.values`, `web.textencoder.subviews` and `web.textdecoder.streaming`: vary chunk partitions, BOM flags, malformed/truncated input, alias mutation, reset/flush calls, and state held across suspension/cancellation. Partition-and-rejoin and encode/decode round trips require valid scalar strings and explicit BOM preconditions. Node comparisons stop at the first fatal error; retained-input recovery uses the Encoding Standard oracle. Generate constructor/decode option effects and preserve source order while shrinking. Include task-memory reuse and independent encoded storage. Fixed-program random byte/scalar inputs do not satisfy this program-search item.
  - Extend filesystem program generation from `node.fs.metadata_lifecycle` and the pending transfer contracts: vary preopen layouts, path spellings, directory depth, typed Stats storage, literal-option effects, success/error variants and file/stream transfers. Equivalent normalized paths must preserve observations inside the same selected capability; only compare mutation reorderings on disjoint paths. Use a numeric WASI error oracle for accepted deviations from Node. Generate deterministic completion/cancellation schedules, including cancellation after return but before dispatch, large/empty `other` strings and concurrent CLI imports. Preserve mount selection, borrowed-descriptor lifetime and relevant aliases during shrinking; verify no cancelled continuation effects, dropped resources and memory plateaus. Directed ownership tests and varying directory depth alone do not complete P6.5.
  - Generate JSON programs combining parse/stringify, duplicate and numeric keys, checked scalar/aggregate narrowing, alias mutation, missing fields, exceptions and suspension. Preserve shared storage and property creation order during shrinking; compare parse/stringify normalization and operations through typed versus JSON views where their preconditions hold. Include repeated references under duplicate keys and absent optional arrays. `ecma.json.values`, `ecma.json.native` and `ecma.json.typed` supply seed cases and data generation; they do not satisfy whole-program or metamorphic search. Generate narrow–mutate–read sequences with repeated views, aggregate replacement, cycles, shared DAGs, growth and suspension. Compare valid typed/JSON access paths with Node; use the subset error contract for stale incompatible views. Shrink while retaining the alias that invalidates a view, and check result-boundary failures and task reclamation. Extend dictionary search with collision-heavy keys, growth/compaction, delete/reinsert, present-undefined versus absent properties, enumeration snapshots, numeric-key boundaries and mutations through views held across await. Metamorphic relations must preserve both values and own-key order: repeated overwrite is order-preserving, while delete/reinsert is only equivalent when the resulting position is unobservable. Use `ecma.json.dictionary` as another seed, not as completion of whole-program search.
  - Enforce exclusion diagnostics through generated source forms as well as directed witnesses. Do not regenerate deferred JSON callbacks or require Node microtask ordering.
  - Add async state-machine generation for already-settled and pending promises, multiple/repeated waiters, rejection through finalizers, overlapping exports and cancellation. Vary host completion order deterministically, use the accepted inline scheduling oracle, and verify task/resource reclamation. For P3 output, vary partial-write sizes, immediate/pending transitions, consumer closure and completion-future errors, including multiple queued lines and overlapping exports. Add allocation/fuel growth checks separately from value equivalence.
  - Extend typed dictionary generation from `ecma.object.dictionary`: vary scalar/optional/aggregate element types, recursive schemas, dictionary-to-record views and invalidating JSON aliases. Compare keys/value snapshots and mutation through each compatible view; preserve schema and alias constraints while shrinking. Include arbitrary host IEEE-754 payloads at numeric-to-optional boundaries, checking definedness and signed zero separately from JSON output. This seed contract still generates data for fixed programs; it does not complete program search or metamorphic testing.
  - Generate constrained schemas from `ecma.object.constrained_dictionary`, varying named refinements, required optional-valued fields versus optional fields, recursive references, and index-signature placement in interfaces, aliases and inline annotations. Moving a signature before/after named fields must preserve behavior. Generate widening/narrowing and invalidating mutations through parsed and native aliases; preserve required presence, the invalidating alias and observable key order while shrinking. Check unused invalid reads/constructions still throw and required native fields survive rejected writes/deletes. This fixed-program seed does not complete the schema/program search.
  - Extend string program generation from `ecma.string.utf8`: compose indexing, slices, split pieces, casing, searches and branched appends with native/JSON views and explicit versus inferred types. Use BMP scalars for Node differential programs, and a separate scalar oracle for astral cases. Vary missing/NaN/signed-zero values and argument effects. Check partition/rejoin at scalar boundaries, trim/case idempotence, repeat lengths and immutable aliases. Preserve optional provenance and invalidating operations during shrinking. Retain spans across suspension/cancellation and verify task-memory reuse. No surrogate-half emulation or WTF-8 generation. Fixed-program data generation remains a seed, not completion of whole-program search.
  - Extend enumeration program generation from `ecma.object.enumeration`: vary native/parsed sources, positional tuple types, field presence, numeric/arbitrary keys, heterogeneous JSON values and native byte references. Check that keys match entry keys, values match entry values, repeated snapshots agree before mutation, and scalar replacement leaves earlier snapshots fixed while nested mutation remains shared. Preserve alias graphs, tuple positions, own presence and key order during shrinking; verify invalidating views throw even when unused. Combine snapshots with suspension, cancellation and task reuse. Use BMP for Node differential cases and explicit scalar expectations for astral text. This fixed-program data generator does not complete generated-program or metamorphic search.
  - Extend record schema/program generation from `ecma.object.optional_record`: independently vary field presence and undefined values, optional versus required declarations, recursive references, source construction order and mutations through native/parsed aliases. Check overwrite and already-absent deletion as order-preserving relations; assigning undefined to an absent field preserves JSON serialization only when own presence and keys are unobserved. Compare operations through compatible typed and JSON views, retain invalidating aliases and presence distinctions while shrinking, and exercise snapshots across suspension plus required-field checks at result boundaries. The fixed-program contract remains a seed for P6.5.
  - Extend the directed component-caller cancellation harness to generated schedules: cancel during a pending write or completion read, mix channels and timers, and keep another export live. Reuse a component instance after cancellation; assert dropped handles, no cancelled continuation effects and a stable memory plateau. Include delayed cancellation acknowledgement to expose freeing buffers before the host releases them.
  - Persist seeds, generated source, feature profile, compiler revision, host trace and oracle output on every failure. Shrink syntax, values and schedules while preserving typing and the failure; provide one-command replay and retain minimal failures as regression fixtures.
  - CI runs a reproducible bounded seed corpus plus a rotating seed, at several depths. A longer opt-in campaign explores deeper programs. Report executed programs/transformations, feature-pair coverage, depth and discarded cases, so a passing campaign cannot hide an empty or mostly-rejected search.
  - *Done when:* CI catches deliberately seeded compiler/runtime mutations (exception-edge loss, aliasing, optional coercion, premature task release), shrinks and replays them; baseline and deeper campaigns pass; all discovered failures are retained or explicitly triaged.

### P7. Cutover and deletion

- [x] **P7.1 / P7.2 Cutover and deletion: superseded on 2026-10-09.** The legacy backend was deleted before feature parity, by decision, and the targets below now apply to P6.2 instead:
  - The temporary-record loop at least matches AssemblyScript (anecdotal, by hand).
  - Example component sizes must return to at or below `3dbf57a`. The early cutover met this for `merge_task` and the template, but subsequent runtime growth reopened the gate; see the P4.3 data-pruning follow-up.
  - HTTP default reader and BYOB are at or better than the P0.2 legacy baseline.
  - Stream workloads stay within the 64 KiB guest limit.
- What was kept from the legacy tree:
  - `link.rs`, `libraries.rs` and `type_compaction.rs`, now in `src/link`;
  - `audit.rs`, `sdk/`, `component/`, `abi/` and the helper crates;
  - the handler WIT, now `crates/conformance/wit/handler`.
- `source/` (multi-module loading, WIT import binding, command adapter) and the `text_contract` rules were deleted. They return with P1.2 and P6.2 group 10.
- [x] **P7.3 Update ARCHITECTURE.md.** Rewritten for the MIR pipeline. Async (D5) and the collector are described once they exist.

### P8. WasmGC decision spike (after P7)

- [ ] **P8.1 Lower `heap.alloc` to `struct.new`/`array.new`** for all three non-scalar tiers, behind `--heap=wasmgc`. Keep `buffer` in linear memory. Run under Wasmtime `-C collector=copying` (DRC does not reclaim cycles).
- [ ] **P8.2 Decide using the P0.2 suite.** Adopt WasmGC only if all of these hold:
  - execution is at most the linear-memory figure on every benchmark;
  - total memory (linear plus GC heap) is at most 1.25× the linear-memory figure;
  - component size is at least neutral;
  - BYOB stays zero-copy;
  - jco and Wasmtime both run it;
  - it deletes at least P5.2, P5.3 and P3.9.

  Record the decision in ARCHITECTURE.md.

## Global acceptance

- No `waffle` dependency (bonus goal), and no `[thread-*]` imports in any emitted component.
- One lowering from HIR to MIR. Every other transformation is a registered pliron pass with a verifier run between passes in debug builds.
- Every value's tier is explainable (`--explain-tiers`). `task` growth is statically bounded by the loop-growth rule. The collector cost is linear in live `instance` data plus the reference-holding `task` objects.
- Workspace tests, conformance (`check`), Clippy, fmt, SDK and example builds, and `nix flake check` pass.

## Open questions

- **Exceptions (decided for now):** the status convention. A function that may throw returns an extra `i32` status after its results, callers branch on it, and the codes are `perry_codegen::status_code`. Wasm exception handling stays a later lowering swap; the MIR's throwing ops are marked, so either lowering works.
- **pliron is pre-1.0:** pin it, and budget for upstream contributions (the inliner, SROA). The Wasm dialect and emitter live in `crates/pliron-wasm`, which is free of perry code and can be published.
- **Strings (accepted profile, 2026-10-09):** UTF-8 with cached Unicode scalar counts and an in-place append buffer. Node differential generators stay within BMP; astral strings use scalar semantics. Lone surrogates, WTF-8 and UTF-16 code-unit APIs are excluded. Benchmark an index cache or a nix-wit-style rope only if a real workload justifies its complexity.
- **R5 scope:** which array and promise builtins accept inlined arrows in the first release.
