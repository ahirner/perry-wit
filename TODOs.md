# MIR completion

Status: acceptance passed for the declared profile within the authorized build policy.
Application baseline revision: `9ac4817`; tested source SHA-256: `b86b87115b19e99cd4405ba7cc4117f32fa1d4a19733f4496ca2303f6e0aa714`.
Temporal validation source SHA-256: `e4f46f39089fc8e3707a81969cf6671ec66f5b254051a66977494a6d10dcfd37`.
Keep the modular runtime, talc, precise non-moving collector, stackless P3 callbacks and stream reuse.
Application correctness, bounded retained memory and maintainability define completion.

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

Consumer validation is authorized for the isolated `1d65d97` snapshot. Leave its repository and
development servers untouched. A different consumer snapshot requires fresh approval.
Stop expanding scope when the completion gates pass.
