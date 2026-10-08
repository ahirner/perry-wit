# TODOs: MIR refactor, scalable memory, stackless async

Status: plan, 2026-10-08. Baseline is `3dbf57a`.
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
  - No property addition, `delete` or `Object.assign` into existing records.
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
- [ ] **P0.2 Freeze the benchmark baseline.**
  - Extend `scripts/compare_components.mjs` with: the critique microbenchmarks, component sizes for all examples, and committed linear-memory peaks.
  - AssemblyScript comparisons stay out of the repo. Run them by hand for anecdotal checks only.
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
    - Twelve tests pass.
    - Remaining:
      - dict/JSON, `iface.*`, bytes;
      - `perry.canon` and `perry.intrinsic`. The plan for `perry.intrinsic` is one table-driven op, not one op per capability.
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
    - Next: async (`perry.async_call` and `perry.await`), globals and module state, truthiness, string comparison, labeled control flow, try/throw.
- [ ] **P2.3 MIR snapshot tests** replace the 10 `waffle_ir` references in the test suite.

### P3. Pass pipeline

Reused from pliron:

- [ ] **P3.1** Configure SCCP, DCE, `simplify_cfg` and `mem2reg` in the pass manager.
  - *Status:* `perry_mir::pipeline::canonicalize` runs mem2reg, then DCE and `simplify_cfg` to a fixpoint, verifying before and after. SCCP still needs constant-folding interfaces on the `perry.*` ops.

Owned (each with unit tests on MIR snippets):

- [ ] **P3.2 Inliner.** Bottom-up, with size budgets. Always inline R5 arrow arguments and runtime accessors.
- [ ] **P3.3 Escape summaries.** Per function parameter, computed to an optimistic fixpoint (Zena `escape.zena`). Store, return, throw, capture into module state, passing to an escaping parameter, identity comparison and import arguments all count as escapes.
- [ ] **P3.4 SROA.** Non-escaping `alloc` dissolves into SSA values and block parameters (Zena `sroa.zena`).
- [ ] **P3.5 Integer ranges.** Loop counters, indices and lengths become i32/i64 where proven. Otherwise use f64.
  - Where `0 <= i < length` is proven, rewrite `perry.array_get` to an unchecked read, drop `perry.optional` and fold the `perry.unwrap`/`perry.to_number` uses (R8).
  - Use `perry.is_undefined` guards as range facts on the defined branch.
- [ ] **P3.5b Throw edges.** Ops implementing `MayThrowInterface` (`perry.call`, `perry.await`, `perry.unwrap`, `perry.array_set`) get explicit exception successors to the nearest handler or the function's exceptional exit. Runs before codegen, after P3.5 has removed provably safe checks.
- [ ] **P3.6 Shapes and interface views.**
  - Exact layouts for closed types: references 4 bytes, f64/i64 8-aligned, packed booleans, optional-presence bits.
  - Per-(shape, interface) field-offset tables.
  - Union discrimination by shape id.
- [ ] **P3.7 Tier classification.** Implements D3, including the loop-growth rule. Emit a per-site report: `perry-wit --explain-tiers`.
- [ ] **P3.8 → Async split.**
  - Each async function becomes a ramp (allocate the frame in `task`, spill arguments, run eagerly to the first pending `await`) and a `step` state machine.
  - Frame fields are the values live across each `await`, taken from pliron liveness.
  - An already-settled `await` continues inline with no host turn.
  - Combinators register the frame as waiter and allocate nothing per operand beyond a node in `task`.
- [ ] **P3.9 → Root planning.**
  - Interprocedural may-collect summary.
  - Spill sets at may-collect call sites are the `instance` references in the liveness set.
  - *Done when:* tests prove there are no spills in allocation-free loops.
- [ ] **P3.10 Canon lowering.** Lower `perry.canon` to buffer-tier allocations with explicit owners, replacing the schema-guided GC tracing of canonical layouts. Port the layout logic from `wit/adapter*.rs`.

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
- [ ] **P4.3 Keep and generalize `link.rs`.** Link `perry-runtime.wasm` the same way as the helper libraries (`__perry_helper` imports, PIC placement, table relocation). Run `wasm-opt -Oz` as an optional post-link step only if it stays a dev or optional tool dependency.
- [ ] **P4.4 Remove `waffle`** from `Cargo.toml`. Delete `constant_arguments.rs`; P3.1 SCCP plus the inliner replace it. *Done when:* `cargo tree -i waffle` is empty.

### P5. Runtime as Rust (`crates/perry-runtime`, `no_std`, compiled like the helpers)

- [ ] **P5.1 Buffer allocator.** `talc` (decided in D3) behind `cabi_realloc`. Owner records for call, transfer and stream. *Done when:* canonical return areas and stream buffers are released at owner end, which the tests check.
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
  - Run queue of frames, using an intrusive list.
  - Waiter table mapping waitable handle to frame and state.
  - Callback dispatch and the export entry, which sets `context.set 0` to the task.
  - Never return YIELD except under a fairness budget.
  - Cancellation fan-out with sync `subtask.cancel` and sync `stream.cancel-read`/`-write`.
- [ ] **P5.5 Port the remaining `.wat` runtime** (about 1.5k lines: utf8, decoder, json, errors, filesystem, http handler, random, regex search) to Rust or to MIR builders. Delete `runtime/builder.rs`, the fixed-address scheme (addresses 0–152) and every `const OFFSET` layout. Layouts come from P3.6 or from Rust structs.

### P6. Feature migration onto the MIR backend (dual backend, whole program)

- [ ] **P6.1 Add `--backend=mir|legacy`.** Default stays `legacy`. Conformance runs both. MIR rejects unsupported programs with a diagnostic and never falls back silently.
  - *Next:* the core module already exists (`perry_codegen::compile`, linked against `perry_codegen::RUNTIME_WASM`). What remains is the component boundary:
    - link the runtime into one core module (P4.3);
    - lower WIT parameters and results, including the `{ value, trace }` records the scaling contracts return;
    - map the status result to the export's error result;
    - strings (`perry.string_*`, `perry.to_string`).
  - Smaller MIR follow-ups found while emitting:
    - remove a `perry.unwrap` dominated by another unwrap of the same value (P3.5b). `point.x; point.y` currently checks twice;
    - inline `fmod` for proven integers (P3.5). `perry_f64_rem` is the largest per-iteration cost in the scaling loops.
- [ ] **P6.2 Migrate in registry order.** Each group is done when its conformance contracts and the matching `tests/*` suites pass on MIR:
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
- [ ] **P6.3 Strip thread flags everywhere.** Remove `-W component-model-async-stackful`, `-threading` and `-more-async-builtins` from README, `scripts/`, `flake.nix`, `crates/conformance/src/execution.rs`, the template, and the roughly 20 test files that pass them. Also remove `thread-*` expectations from `tests/fixtures/cancellation/callee.wat` and `tests/support/call_counts.rs`. *Done when:* `rg -n 'thread-|stackful|threading|more-async-builtins'` returns nothing outside history.
- [ ] **P6.4 Portability check.** Emitted components validate with wasmparser **default** features, so no 🧵, 🚟 or 🚝. Optionally smoke-run `merge_task` and a CLI example in jco.

### P7. Cutover and deletion

- [ ] **P7.1 Switch the default to `mir`** once every contract and test passes and the P0.2 targets hold:
  - The temporary-record loop at least matches AssemblyScript.
  - Retained records at 1e6 take at most 2× AssemblyScript.
  - HTTP default reader and BYOB are at or better than the P0.2 baseline.
  - Example component sizes are at or below `3dbf57a`.
  - Stream workloads stay within the 64 KiB guest limit.
- [ ] **P7.2 Delete the legacy backend.** Line counts are approximate, at `3dbf57a`.

  | Area | Lines | Fate |
  |---|---|---|
  | `ssa.rs`, `ssa/*` | ~8.7k | Replaced by P2.2 and P3 |
  | `allocation*` | ~1.2k | Replaced by P5.1–P5.3 |
  | `runtime/{scheduler,callbacks,operations,transfers,subtasks,pending_result,builder}` | ~1.7k | Replaced by P3.8 and P5.4 |
  | `promises/*` | ~1k | Replaced by `perry.async` |
  | `objects*`, `values*`, `nullable`, `type_compaction`, `structured*` | – | Replaced by P3.6 |
  | `constant_arguments` | – | Removed (P4.4) |
  | `*.wat` | – | Removed (P5.5) |

  - Keep: `link.rs`, `libraries.rs`, `sdk/`, `component/`, the helper crates, `source/`, and the `text_contract` rules, which move into the P1.2 checker.
  - Rename `waffle_backend` to `backend`, and the `waffle_*` test files to capability names.
- [ ] **P7.3 Update ARCHITECTURE.md:** pipeline, D3 tiers, D5 async, R1–R7.

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
- **Strings:** rope or builder lowering for append loops, which already beat AssemblyScript but are super-linear.
- **R5 scope:** which array and promise builtins accept inlined arrows in the first release.
