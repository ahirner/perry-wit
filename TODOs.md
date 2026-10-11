# MIR completion

Status: acceptance passed for the declared profile within the authorized build policy.
Application baseline revision: `9ac4817`; tested source SHA-256: `b86b87115b19e99cd4405ba7cc4117f32fa1d4a19733f4496ca2303f6e0aa714`.
Temporal validation source SHA-256: `e4f46f39089fc8e3707a81969cf6671ec66f5b254051a66977494a6d10dcfd37`.
Keep the modular runtime, talc, precise non-moving collector, stackless P3 callbacks and stream reuse.
Application correctness, bounded retained memory and maintainability define completion.

## Performance completion

Reference: primary `e83eaf1`; starting implementation: `3c3442a`. Preserve the verified
semantics, precise collector and P3 ownership rules. Each optimization has its own commit,
Perry measurements, all 50 conformance contracts, the fixed generated campaign and both
consumer commands on isolated `f655369f00042c37dc5f5f297050f7ed0085c52e` snapshots.

0. [x] Freeze a serial debug-build benchmark and consumer baseline.
1. [x] Elide root bookkeeping for proven non-collecting calls; emit root-stack stores inline.
   [Full campaign](bench/performance/reports/01-roots/summary.json) passes all contracts,
   four seeds, 55 consumer tests and 25 benchmark scenarios.
2. [x] Lower integer remainder inline with IEEE-754 fallback and negative-zero preservation.
   [Full campaign](bench/performance/reports/02-remainder/summary.json) passes all contracts,
   four seeds, 55 consumer tests and 25 benchmark scenarios.
3. [x] Extend bounded leaf inlining, closed-record argument promotion and scalar replacement.
   [Full campaign](bench/performance/reports/03-aggregates/summary.json) passes all contracts,
   four seeds, 55 consumer tests and 25 benchmark scenarios.
4. [x] Prove indexed accesses and reuse dominating successful optional unwraps.
   [Full campaign](bench/performance/reports/04-ranges/summary.json) passes all contracts,
   four seeds, 55 consumer tests and 25 benchmark scenarios.
5. [x] Reuse string headers when ownership and liveness permit mutation.
   [Full campaign](bench/performance/reports/05-strings/summary.json) passes all contracts,
   four seeds, 55 consumer tests and 25 benchmark scenarios.
6. [x] Grow exclusive unpinned buffers in place with collector-aware metadata updates.
   [Full campaign](bench/performance/reports/06-growth/summary.json) passes all contracts,
   four seeds, 55 consumer tests and 25 benchmark scenarios.
7. [x] Port local coalescing, emitter peepholes and async branch-table dispatch (`c68360b`).
8. [x] Complete workspace, SDK/example, lint and permitted Nix validation on frozen source.
   All 234 workspace tests, formatting, strict host/Wasm Clippy, flake evaluation and the
   Nix WIT/format checks pass. Package checks requiring a release compiler remain excluded.

The benchmark harness is `tests/benchmark.rs`; `bench/performance/run.py` runs campaigns.
Reports include source/tool identities and normal fuel separately from instrumented counters.
Historical speedups are hypotheses: the existing temporary-record workload already allocates
no records, and its remainder cost must be measured independently. The reported primary
append improvement is 149M to 84M fuel. Keep existing baseline files unchanged.

### Consumer profiling

Starting source: cleaned branch `3c5541f`; approved consumer snapshot: `f655369`.
Use the debug compiler and preserve the existing baseline reports.

- [x] Preserve linked names for profiling; measure startup-inclusive AOT separately from
  direct invocation, fuel and cold JIT compilation.
- [x] Reuse eligible string headers, ASCII characters and intact split results. Inline short
  literal comparisons and compare other strings by equal eight-byte words.
- [x] Reduce root publication, JSON measurement, UTF-8 decoding and pinned-buffer lookup work.
- [x] Omit unreachable functions, inline nonthrowing leaf builtins and preserve deterministic
  root/frame ordering. Keep the bounded runtime optimized for size.
- [x] Remove repeated collector page lookups and isolate one-time root initialization.
  Direct static root storage failed the small-component budget and was fully removed.
- [x] Walk UTF-8 casing directly and skip unnecessary Unicode property lookups.
- [x] Initialize collectible payloads once; preserve zeroed new storage and reclaimed-storage poisoning.
- [x] Profile fresh instances and component reload separately from component execution.
- [x] Validate the retained source: 245 workspace tests, all 50 contracts, four fixed
  program seeds, 55 consumer tests, 25 benchmarks, strict host/Wasm Clippy and permitted Nix
  checks. [Measurements](bench/performance/reports/profile-33-payload-init/summary.json) and
  [quality evidence](bench/performance/reports/profile-34-validation/summary.json).
- [x] Reach 2× on directly measured component execution, excluding startup:
  7.207 → 2.843 ms (2.535×). All 25 scenario outputs match across 15 recorded
  paired rounds per scenario. [Acceptance evidence](bench/performance/reports/profile-33-payload-init/acceptance.json).

Timing definitions, source identities, rejected trials and reproduction commands are in
[bench/performance](bench/performance). Separate full-process AOT medians are
13.530 → 8.617 ms (1.570×). Cold JIT median is 209.7 ms versus 281.4 ms at the
profiling baseline. The component-execution performance goal is complete.

### MIR consolidation

- [x] Keep eligible character reads, comparisons and builder appends in scalar MIR form.
  Preserve ordinary strings for unsupported or escaping uses and retain append ownership proofs.
- [x] Centralize allocation contracts in MIR operations and intrinsic declarations. Remove
  codegen's duplicate atomic-helper whitelist while preserving conservative caller roots.
- [x] Share append buffer handling and retain the existing small-component size budgets.
- [x] Preserve or improve direct component execution against `8727f47`: 2.809 → 2.736 ms
  (1.027×). Unicode builder fuel falls 139.3M → 70.5M; ordinary append falls 121.8M → 120.8M.
- [x] Pass 247 workspace tests, all 50 contracts, the four fixed program seeds, 55 consumer
  tests, 25 benchmark scenarios, strict host/Wasm Clippy and permitted Nix checks. All 19
  rebuilt consumer components match the measured artifacts byte for byte.

[Acceptance evidence](bench/performance/reports/profile-37-scalar-mir/acceptance.json) and
[quality checks](bench/performance/reports/profile-38-validation/summary.json) record the frozen
source, debug compiler, direct timings, deterministic fuel and preserved memory budgets.

[Starting measurements](bench/performance/reports/00-baseline/summary.json): all 50 contracts,
400 generated cases, four fixed program seeds, 55 consumer matrix tests and 25 consumer
benchmark scenarios passed. Temporary records allocate zero objects. Direct calls cost
231M fuel, remainder 186M, incoming streams 1.221B and string append 775M. HTTP transfer
partitioning can vary with host scheduling; compare deterministic core workloads separately.
Consumer median wall times: Node 3.8 ms, AOT 16.7 ms, JIT 268.6 ms. Raw outputs and snapshot
harness adjustments accompany the report; startup-subtracted estimates are not guest timings.

Root optimization focused checks pass: 28 codegen, three stream and 14 async/import/resource/
cancellation tests. Direct-call fuel is 24.0M versus 231.0M at baseline; numeric arrays 372.7M
versus 473.7M; append 632.9M versus 774.9M. Core peak memory is unchanged. Root publication counters
exist only in checked modules; the ordinary descriptor has three words and preserves the
retained-chunk and 2 MiB-through-64 KiB tests. Collection thresholds are unchanged.

Remainder focused checks pass (30 codegen tests): the scalar loop falls from 186.1M to 35.0M
fuel, and temporary records from 188.1M to 37.0M, still with zero record allocations. Components
grow by 48–51 bytes where the fast path is used; other deterministic costs are unchanged.
The effect audit also accounts for the empty string header allocated when a caught non-error
value's message is read; forced collection covers this exception path.

Aggregate focused checks pass (33 codegen tests). Multi-block leaf inlining keeps the
256-operation caller budget; read-only record parameters become field snapshots only when
mutation and reentry are excluded. Direct-call fuel falls from 24.0M to 22.0M and the temporary
record component shrinks from 12,353 to 4,771 bytes without changing zero-allocation behavior.
Constant branches remove unreachable blocks before verification; lowering follows CFG order.
The full post-commit campaign passed.

Range/unwrap focused checks pass (35 codegen, nine BYOB and three stream tests). Dominating successful
unwraps are reused before bounds analysis so identical byte views share one SSA value.
Incoming-stream fuel falls from 1.221B to 461.7M at 64 KiB; byte indexing from 478.1M to
333.1M; numeric arrays from 221.6M to 191.6M. Optional elements, invalid indices, caught
failures, detached aliases and suspension retain their checks. Unchecked byte reads preserve
stream leases. The full campaign passed.

String-header focused checks pass (37 codegen tests). The million-append workload falls
from 632.9M to 168.0M fuel, 1,000,015 allocations to 17, 74 collections to five, and 3,735,552
to 2,555,904 peak bytes. Retained prefixes and scalar counts survive forced collection.
The append component grows 412 bytes; non-string core workloads gain about 61 bytes of linked
runtime cost. Retained-record fuel rises 0.024%, with identical allocation/collection counts
and peak memory. The conformance and consumer campaign passed; stage 6 restores the small-append size gate.

Buffer growth focused checks pass: non-moving expansion crosses directory-table boundaries,
new pointer storage is zeroed and traced, fragmentation/pins force copying, and denied growth
preserves existing bytes. Forced collection, cancellation, retained chunks and the 64 KiB
stream gates pass. Directory tables initialize directly on the heap; fixed-alignment layout
checks and compact append helpers preserve both frozen small-component size budgets.
The final preview reduces HTTP accumulation from 48.2M to 35.0M fuel and 25,427,968 to
15,794,176 peak bytes. Numeric arrays fall from 191.6M to 187.3M fuel and 17,367,040 to
15,990,784 bytes (11 in-place growths, seven fallbacks). Retained-record fuel rises 0.37%
with unchanged peak memory: interleaved live allocations require copying, and the more general
allocation path adds a small fixed cost. Growth adds about 1.2 KiB to array/stream components;
allocation-free components remain small. Collection thresholds are unchanged. The full campaign passed.

Emitter focused checks pass: 13 Wasm dialect tests, 38 codegen tests, real timer suspensions
with cyclic values and poisoned reclamation, imports, the depth-32 guard, size budgets and stream
reuse. Coalescing follows emitted liveness, including folded operands; branch arguments retain
simultaneous assignment. Debug local names remain unique, and malformed branch tables are diagnosed.
The preview lowers temporary-record fuel from 37.0M to 30.0M, remainder from 35.0M to 28.0M,
numeric arrays from 187.3M to 175.3M, streams from 461.7M to 453.3M, and append from 167.9M
to 161.9M. Allocation counts and peak memory are unchanged. Component/core sizes decrease by
14–643 bytes in the measured workloads; depth-32 compilation remains about 10 ms. Final quality
checks pass; the full post-commit campaign is pending. Lowering regressions retain exported
record shapes and recursive calls so optimization cannot erase their test subjects.

## Verified implementation

These checks passed on frozen source. [Application baseline measurements](bench/baseline/completion.json)
record tool versions, component costs, memory bounds, scaling and consumer results.

- [x] JSON null/missing comparisons, nullable narrowing and required-value checks preserve
  distinct `null` and `undefined` when the declared type admits both.
- [x] Conservative allocation effects, bounded inlining and scalar replacement. Proven local
  values use bounded frame storage; unproven allocations use collectible storage.
- [x] Size-class pages over talc, exact pointer maps, side mark bits, direct allocation lookup,
  explicit tracing, free lists and lazy sweep. Dead frame slots and completed operation edges clear.
- [x] Allocation-volume collection requests use `max(256 KiB, live bytes after collection)`.
  Compiled safe points service requests and allocator pressure; helpers remain atomic.
- [x] Module cells, compiled values, suspended frames and active operations supply roots.
  Views retain owners; host transfers pin storage until completion/cancellation acknowledgement.
- [x] `--explain-tiers`, allocation/collection/live-byte/transfer-copy counters, forced collection
  and reclamation poisoning support focused ownership checks.
- [x] 200,000 temporary-record iterations fit within 512 KiB. Overwritten references and completed
  async operations reclaim; retained graphs and module-state cycles survive collection.
  Unchanged module graphs avoid collection on every allocation-free export.
- [x] Static modules, synchronous initialization, persistent typed state and named WIT imports.
- [x] Application boundary records, lists, tuples, options, enums, finite variants, tagged results
  and exact s64/u64 bigint transport. Completion-style exports map compatible returns/throws to
  results; explicitly declared tagged results take precedence.
- [x] Opaque resources, transactional argument transfer, borrowing, alias invalidation and
  exactly-once release, including nested result/resource lists.
- [x] Recursive `WitInput<T>` accepts strings or Uint8Arrays for outbound byte lists. Strings use
  existing UTF-8 payloads; canonical inbound storage is adopted where ownership permits.
  Mutable bytes remain independent of immutable strings.
- [x] Eager async calls, inline settled awaits, repeated observations and promise combinators.
  Host cancellation acknowledges transfers before task release; detached work receives cleanup
  and a task-boundary diagnostic.
- [x] Primary's promise-timer AbortController behavior: pre-aborted signals, pending cancellation,
  completed timers and idempotent abort. Active watches reuse scheduler entries and detach on completion.
- [x] Writer close waits for queued writes and propagates failures. Acknowledged stdout buffers
  reuse storage; stream iteration releases readers on early exit or failure.
- [x] 2 MiB streaming within 64 KiB, plus the saved 4 MiB read/write regression repeated three
  times within 64 KiB. Retained chunks and pending reader cancellation have focused coverage.
- [x] BYOB buffer transfer, alias detachment, offsets, minimum reads, short EOF, queued reads,
  cancellation and release. Main's regressions pass with forced collection/poisoning; combined
  reads/writes fit within 64 KiB under ordinary collection. Stream and bounded-fetch differential
  contracts pass; default-reader reuse and incremental decoding remain covered.
- [x] Deterministic filesystem/output cancellation fixtures from `03ea`: before transfer,
  partial transfer, pending completion and completion/error races. Unused canonical strings release;
  short writes reject. Repeated calls verify stable memory and exactly-once handle release.
- [x] Host fetch cancellation before response arrival and during partial bodies. Forced collection
  and poisoning cover stream overlap, imports, results and resource borrowing/transfer.
- [x] Closed checked JSON conversion, native aggregate independence, UTC dates, bounded literal
  regexes, dense arrays, byte operations, strict/incremental UTF-8 decoding and static control flow.
- [x] ISO Temporal factories, fields, formatting and plain day arithmetic, using the existing time
  codec and collectible storage. The supported API is listed in [README](README.md#semantics).
  Values survive forced collection, poisoning and timer suspension; two 200,000-iteration
  allocation runs stay within 512 KiB. Error construction adds no shared helper data.
- [x] HTTP(S) URL construction, protocol/credentials/fragment inspection, pathname mutation,
  serialization, linked `searchParams.append`, `encodeURIComponent` and `fetch(URL)`.
- [x] UTF-8 scalar indexing, ASCII fast paths, shared immutable slices, amortized append,
  retained-prefix correctness and sequential non-ASCII cursor caching.
- [x] Real checked-in examples, including `merge_docs`, execute against their WIT worlds with
  overlapping HTTP calls and structured error results.
- [x] Bounded typed program generation covers functions, control flow, aggregates, byte aliases,
  JSON conversion, exceptions and timer schedules. Unexpected rejection fails the campaign.
  Existing shrinking/replay and valid expression/control-flow/exception transformations remain.
- [x] MIR verification after transformations; emitted Wasm validation without threading,
  stackful async or additional async builtins.
- [x] Consumer regressions cover interface-scoped WIT names, local type precedence, quoted
  closed fields, scalar truthiness/indexing, JSON key lookup, checked tuple arrays and strict
  JSON/scalar equality. Closed aggregate absence widening preserves aliases without copies.
- [x] Canonical indirect parameters work for synchronous and asynchronous exports. Async
  returns adopt promises after finalizers and propagate later rejection outside surrounding catches.
- [x] Controlled linear workloads stay within 6x fuel for 4x input; depth-32 compilation guard.
  Identity/append components stay within 10% of frozen sizes and one initial memory page.

## Acceptance

1. [x] Frozen measurements retain the original `bench/baseline/measurements.json` comparison.
   Identity/append remain within 10% of baseline. Temporary records, builtin churn and retained
   cycles use 64 KiB in the measured workloads. JSON/application costs are recorded separately;
   unavailable historical comparisons remain unverified.
2. [x] 207 workspace tests, all 50 conformance contracts, SDK/examples, formatting and strict host/Wasm
   Clippy pass. Conformance includes `ecma.temporal.iso`, 400 generated cases and 111 directed
   witnesses with unchanged source; only this status document changed after validation.
   The application baseline also passed four program seeds (`0`, `1`, `42`, `20261010`), eight
   programs each, depth three. Shrinking/replay regressions pass. Flake evaluation and WIT checks
   passed for the application baseline. Full Nix package
   checks require another compiler release build and remain outside the authorized build policy.
3. [x] The approved `1d65d97` consumer snapshot passes 96 Node tests and builds twelve application
   components plus seven test components. Its structural matrix passes 55 tests, including all
   fifty replay/HTTP scenarios, without thread flags. Only `Applied.diff` is parsed before comparison;
   every value and array position remains checked. Native-record key order is the deviation below;
   the original exact-text matrix retains 46 scenario mismatches and two failed parent suites.
   Cold Wasmtime invocations take 257–325 ms; peak process RSS is 49–64 MiB. Every guest linear
   memory fits under a 4 MiB cap; eight Kost scenarios trap at 2 MiB. This cap is not total guest
   high-water memory. Detailed local evidence is in `target/validation/acceptance-report.md`.

Reuse references: primary `62954e7` supplies promise-timer cancellation and `1c072a7` BYOB;
`03ea` at `d21baba`
supplies host cancellation and deterministic filesystem/output phase fixtures; main at `104461d`
supplies BYOB, applicable boundary examples, byte transfer and cancellation ordering. Adapt ownership
to this collector; preserve the primary runtime organization.

Validation limit: Wasmtime 49.0.2 retains a concurrent-table entry for a composed, inline-completing
hand-written callback callee independently of Perry code. Pending-call cancellation passes. Keep the
reduced reproduction with validation evidence; avoid runtime changes to imitate microtask ordering.

## Accepted deviations

- Settled awaits resume inline; continuations follow P3 completion events.
- UTF-8 strings index Unicode scalars. Node differential generation uses BMP scalars; astral text
  and invalid UTF-8 use explicit Perry expectations. Lone surrogates, `charCodeAt` and `fromCharCode`
  receive diagnostics.
- JSON uses explicit checked schemas and callback-free parse/stringify. Revivers, replacers,
  indentation and shared native/JSON aggregate storage remain outside the supported surface.
  Checked native schemas retain declared fields; JSON snapshots retain the complete document.
  Equality between JSON snapshots requires explicit scalar conversion; JSON/scalar equality is strict.
  Native-record JSON keys follow schema order. Consumer JSON payload comparisons use parsed values
  under the authorized conformance reduction; insertion order is outside this native-record contract.
- Static typing covers closed aggregates. General dictionaries, `any` and arbitrary dynamic
  property mutation remain excluded. Regex construction uses bounded literal DFAs.
- Filesystem APIs use `node:fs/promises`; callback and synchronous APIs are unsupported.
- One active stream reader. Host cancellation discards the suspended activation after acknowledged
  cleanup; ordinary source exceptions and control-flow exits execute their finalizers.
- A resource declaration imported and exported on both sides of one world requires distinct source
  identities and is diagnosed. Exported aliases of an imported resource preserve its identity.
- Helpers may have input-dependent temporary allocation peaks and collect at the next safe point.
  Address an individual helper only if a measured acceptance budget fails; persistent accumulation
  across repeated calls remains a defect.

## Deferred improvements

- Implicit async module initialization; synchronous setup plus an explicit async initialization
  export is the supported arrangement.
- Source cancellation across export tasks or beyond the implemented promise timers unless a required
  application uses it. Cancelling another task through a shared signal receives a diagnostic. Safe host/task cancellation remains mandatory for all implemented operations.
- Unused WIT combinations, further escape-analysis expansion and optional optimization.
- Deeper generated campaigns use existing seed/depth/case controls. No separate general fuzzing
  framework or new memory-management architecture is planned.

## Execution

Changes stay in this worktree. Use one build job and test thread, including integration concurrency:
`CARGO_BUILD_JOBS=1`, `RUST_TEST_THREADS=1`, `-j 1`, Nix `--max-jobs 1 --cores 1`.
Use `CARGO_INCREMENTAL=0`; keep pinned Cargo, Nix and documentation toolchains aligned. Debug compiler
builds embed optimized guest helpers; do not start an additional compiler release build.

Performance validation is authorized for isolated `f655369f00042c37dc5f5f297050f7ed0085c52e` snapshots;
the earlier application acceptance used `1d65d97`. Leave its repository and
development servers untouched. A different consumer snapshot requires fresh approval.
Stop expanding scope when the completion gates pass.
