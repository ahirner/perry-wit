# TODOs: MIR refactor, scalable memory, stackless async

Status: incomplete. Baseline: `3dbf57a`; legacy implementation and tests: `45e8788`.
The MIR pipeline is the only backend. Removing WAFFLE was completed before feature parity.

## Scope and priority

Restore the useful baseline before extending the language. The accepted profile is:

- UTF-8 strings with scalar indexing; Node differential generation uses BMP scalars.
  No surrogate halves, WTF-8, `charCodeAt` or `fromCharCode`.
- Settled awaits resume inline. Pending host work uses WASI 0.3 P3 completion events;
  independent operations and export tasks can overlap. No simulated JS microtask FIFO.
- Compact, callback-free JSON. Revivers, replacers and indentation are compile-time errors.
  Schema conversion produces native values or JSON snapshots, without shared native/JSON storage.
- `fs/promises` and `node:fs/promises` only. No synchronous or callback filesystem APIs.
- Bounded streams with one active reader. No BYOB buffer detachment.
- Bounded literal DFA regexes; dynamic `RegExp` construction is rejected.

The typed/untyped dictionary expansion is removed: no constrained index signatures,
optional property presence/deletion, dynamic enumeration, checked aggregate views or
runtime schema descriptors for dual storage. Closed records and dense arrays keep direct
native access. `T | undefined` remains a value type, separate from optional property presence.
Do not restore those extensions as prerequisites for filesystem, streams or fetch.

The immediate order is core arithmetic/Math and UTC dates, bounded streams, fetch and
promise combinators, then the remaining baseline APIs. Literal options suffice while
restoring the main host paths.

## Design decisions

### D1. Typed MIR

Perry HIR → pliron MIR → registered optimization/lifetime passes → Wasm dialect →
`wasm-encoder`. Pin pliron 0.19 at `6ff5b3c`, without LLVM. Use its ops, verifiers,
rewriter, pass manager, liveness, mem2reg, DCE and CFG simplification. Keep unsupported
source forms as diagnostics before emission.

### D2. Wasm emission

The `pliron-wasm` emitter uses dominator-based structured control flow, block arguments,
parallel moves and stack scheduling. The existing linker relocates Rust helpers and prunes
unused code. WAFFLE and the legacy backend stay deleted.

### D3. Memory protocol

| Tier | Intended use | Reclamation |
|---|---|---|
| scalar | Aggregates eliminated into SSA | No allocation |
| frame | Function-local values not live across await | Per-task bump-stack pop |
| task | Values confined to one export, including async frames | Bulk release after task completion/cancellation and host acknowledgement |
| instance | Module state and allocations requiring dynamic liveness | Non-moving tracing collector |
| buffer | Canonical ABI and host transfer storage | Explicit owner and acknowledgement |

`talc` 5.1 remains the buffer allocator, chosen by the allocator benchmarks. Objects stored
into longer-lived objects must be promoted. Loop-carried replacement and overwriting
reference slots must not accumulate dead objects in a task region: prove bounded frame
lifetime or use the instance collector. This rule is not implemented yet.

The intended collector uses size-class pages, shape pointer maps, side mark bits, an
explicit mark stack and lazy sweep. Roots are module globals, reference-containing task
objects, and live instance references spilled at may-collect calls. No generational or
incremental collector and no write barrier are required. Byte views retain their buffer
owner, offset and length. Each overlapping task owns a separate region.

### D4. Language subset

Closed record shapes; dense homogeneous arrays; no `any`; explicit `JsonValue` trees;
no escaping closures; no task-owned async values in module state. Dense misses yield
undefined, a required typed use throws TypeError, and hole-creating writes throw RangeError.
Schema-based JSON conversions copy aggregate values; scalar conversions unbox after a
variant check. Native schemas have a compile-time nesting bound and exclude recursive
conversion. Native serialization uses schema field order. Dynamic JSON trees retain their
own aliases and parsed property order. Cross-representation alias identity is excluded.

### D5. Stackless async

Use P3 callback-lift exports, live values spilled in task frames, eager calls and inline
settled awaits. Host completion events resume pending frames. Cancellation must acknowledge
borrowed buffers before releasing them. No Component Model thread features.

### D6. WasmGC

Deferred until baseline parity and linear-memory lifetime management are complete. Compare
runtime, total memory, component size and portability before changing representation.

## Work items

### P0. Baselines and checks

- [x] Keep reproducible scaling witnesses and pinned dependencies.
- [ ] Recover baseline performance and component-size measurements from `3dbf57a` and
  `45e8788`; compare the temporary-record loop, append, retained records, file/HTTP streams
  and small components. Passing fuel ratios alone do not prove bounded memory.
- [ ] Resolve the Nix `audit-tmpdir.sh` classifier crash. Nix currently reports success despite
  that subprocess warning; do not count the audit as clean.

### P1. Frontend

- [ ] Consolidate source validation and type inference. Preserve evaluation order and report
  unsupported forms before MIR construction; no compiler panics for ordinary builtin calls.
- [ ] Restore source-module loading and SDK/WIT import binding needed by baseline examples.
- [ ] Keep focused rejection tests for the accepted exclusions and closed-shape restrictions.

### P2. MIR

- [x] Scalar/control-flow ops, closed records, dense arrays, bytes, exceptions and async ops.
- [ ] Migrate remaining baseline operations with type/effect verification and textual round trips.
  Do not add dynamic dictionary, enumeration or JSON-view dialects.

### P3. Passes and ABI

- [x] mem2reg, DCE and CFG cleanup; stackless async split with suspension liveness.
- [ ] Inlining and SROA, then escape summaries and tier assignment including loop growth.
- [ ] Integer ranges and redundant bounds/undefined/exception-check elimination.
- [ ] Precise roots at may-collect calls and explainable allocation tiers.
- [ ] Finish canonical ABI coverage required by baseline SDKs. Preserve current primitive,
  record/list, string/byte variant and completion-result boundaries. General imports,
  explicit tagged results, nested/parameter results, options, flags and resources remain open.

### P4. Emission and linking

- [x] Wasm dialect/emitter, helper relocation, WAFFLE deletion.
- [ ] Prune unused runtime data as well as functions; recover baseline small-component sizes.
- [ ] Validate emitted components without thread extensions and smoke-test another host.

### P5. Runtime lifetimes

- [x] Per-task regions, byte owners, P3 timers, output and promise filesystem transfers;
  cancellation keeps borrowed storage live until acknowledged.
- [ ] Frame reclamation, instance collector and module-state roots. Task allocation alone
  is not the final scalable-memory design.
- [ ] Apply the same ownership rules to web streams and HTTP bodies, with bounded buffers.
- [ ] Promise combinators and supported inline callbacks. Do not add a JS microtask scheduler.

### P6. Baseline feature parity

- [ ] **P6.1 Core values.** Arithmetic, Math rounding, dense arrays, records and numeric Date
  construction/getTime pass the existing generated-program corpus. Restore the remaining
  baseline UTC getters/setters and ISO conversion, then broaden core feature combinations.
  Keep compact schema-based JSON; recursive native schemas and dynamic alias views are excluded.
- [ ] **P6.2 Host paths.** Restore bounded ReadableStream/WritableStream operations, locks,
  close/error/cancel and backpressure; then Request/Response/fetch, body consumption and HTTP
  handlers. Reuse native P3 transfer ownership. Verify two fetches can overlap and cancellation
  drops handles without freeing host-borrowed memory early. This takes priority over option
  generalization, stored options and dictionary features.
  Stdio `Writable.toWeb` now uses the existing P3 output queue: exclusive locks, ordered
  acknowledged byte writes, close, release and catchable failures. It requires both stdio
  WIT imports. Default readers, cancellation APIs, backpressure signals and fetch remain open.
  Per-write promises and queue nodes still live until task completion; P5 reclamation is
  required before claiming bounded memory for a long-running stream. Legacy BYOB detachment
  contracts are exempt; default-reader delivery and bounded-memory contracts remain required.
- [ ] **P6.3 Remaining baseline APIs.** Promise combinators, URL/headers, bounded literal regex,
  remaining string/array methods, CLI formatting and filesystem AbortSignal where baseline
  consumers need them. Filesystem remains promise-only.
- [ ] **P6.4 Integration.** Restore `examples/merge_docs.ts`, SDK checks, WIT imports and required
  boundary forms. Unsupported catalog entries remain pending; accepted deviations stay exempt.
- [ ] **P6.5 Generative and metamorphic testing.** Reuse the historical bounded typed program
  generator, IEEE-754 corpus and direct/local/branch/loop transformations. Restrict Node string
  generation to BMP, and use explicit inline-scheduling and numeric-WASI oracles where needed.
  Add combinations of restored core features and deterministic stream/fetch completion schedules.
  Save seed/source/oracle, shrink failures and support one-command replay. Run a small fixed CI
  corpus and an opt-in deeper campaign; retain minimized failures as regressions. Verify that
  seeded arithmetic, control-flow and premature-buffer-release faults are detected. This is a
  practical extension of the existing harness, not a new general-purpose AST fuzzing framework.

### P7. Acceptance

- [ ] Baseline-supported contracts, workspace tests, strict host/Wasm Clippy, formatting,
  SDK/examples and Nix checks pass. Exemptions are explicit; removal of speculative contracts
  must not hide failures of baseline arithmetic, strings, arrays, records, UTC dates or I/O.
- [ ] No WAFFLE dependency or Component Model thread imports.
- [ ] Streaming memory stays bounded, overlapping tasks reclaim independently, and loop churn
  follows D3. Size and performance meet the recovered baseline.
- [ ] Documentation describes implemented behavior and clearly identifies remaining work.

## Validation discipline

Use one build job and one test thread (`CARGO_BUILD_JOBS=1`, `RUST_TEST_THREADS=1`, `-j 1`).
Nix builds also use `--max-jobs 1 --cores 1`. Root debug builds compile embedded Wasm helpers
with release optimization; do not start an additional release build or parallel validation.
Run conformance against frozen source after code changes stop. Record current results in the
commit/hand-off; older passing reports do not validate this correction.
