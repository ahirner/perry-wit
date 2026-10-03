# Perry-WIT Roadmap: Perry HIR → WAFFLE → WASI 0.3

Transform perry-wit into a compiler with a dedicated Perry HIR → WAFFLE backend
and WASI 0.3 (P3) component interfaces. Keep LLVM out of perry-wit's dependency
and link graph. Reuse Perry's frontend and suitable transforms, Rust libraries,
and component binding tools; replace the implementation choices that obstruct
this path rather than preserving the current guest runtime as a requirement.

## Architectural Outcomes

```text
TypeScript + resolved bindings + selected Perry transforms
    → Perry HIR with explicit language semantics and typed capability operations
    → WAFFLE control flow and values
    → core Wasm + the language support that the workload actually needs
    → WASI 0.3 component, built against resolved WIT
    → host async primitives and existing WASI bindings
```

- **P3 is the async component target.** External waiting, I/O readiness, and
  wakeup propagation belong to host primitives wherever the tested ABI permits.
  Guest code still implements language evaluation, supported Promise behavior,
  cleanup, and ownership. An async Rust host call alone is not proof of P3 output.
- **WAFFLE replaces `perry-codegen-wasm`.** Lower HIR deliberately into typed
  values, branches, loops, calls, and exceptional/suspending paths. Today's
  LLVM-oriented `perry-codegen` is not the replacement. Reuse a Perry transform
  only after its output and assumptions work with this backend and language contract.
- **All text is valid UTF-8 with Unicode-scalar semantics.** Apply the contract
  below to source decoding, literals, folding, supporting libraries, runtime values,
  and component/I/O boundaries. This deliberately changes ECMAScript's UTF-16
  string semantics; retain Node comparisons wherever the contracts still agree.
  Define this target now, then implement it on the established WAFFLE path.
- **Capabilities have a small compiler-lowering trait.** Group related typed
  operations by responsibility after resolving their bindings. Shared async,
  stream, value, and ABI mechanisms remain outside individual capabilities.
  Derive the trait from working consumers; it is not a plugin or feature-gating framework.

This roadmap does not require maintaining two compiler backends.

### Language Contracts to Establish

**Strings:** Every string is valid UTF-8. Length, indexing, slicing, iteration,
and supported search/regex inputs and returned positions count Unicode scalar
values. A returned search position must work as an index or slice bound; operations
must never split a UTF-8 sequence. For `"😀x"`, the logical length is **2**, index 0
is `"😀"`, and index 1 is `"x"`; the encoded length is **5 bytes**. Scalars are not
grapheme clusters. Preserve text without automatic normalization, and use
scalar-sequence equality and lexicographic scalar ordering.

Reject malformed UTF-8 and unpaired surrogate escapes in source and JSON while
the input still identifies them; valid paired escapes decode to a scalar. Preserve
familiar coercion and out-of-range behavior where compatible. Arbitrary bytes remain
binary values. ABI byte lengths and API-specific restrictions remain intact;
any deliberately lossy decoder needs a separate, explicit API contract.
UTF-16-specific APIs such as `charCodeAt` and `fromCharCode` need a documented
scalar interpretation or an unsupported-operation diagnostic. Prefer code-point
APIs for consumers; do not retain an internal code-unit string mode.

**Await and Promise:** Begin with immediately awaited, statically known operations
and named async tasks, with an explicit initial concurrency limit. Direct lowering
may avoid constructing a Promise only where it preserves the declared observable
behavior, including evaluation/start order, errors, and await ordering. This also
applies to already-completed operations and `await` of ordinary values when exposed.

A P3 `future<T>` transports one result through an owned readable end; it is not
a reusable JS Promise object. A stored/shared Promise needs its settled value or
rejection retained for repeated awaits and multiple observers, with the supported
reaction ordering. An async WIT function returning `T` and a WIT function returning
`future<T>` are different contracts; do not mechanically translate every
`Promise<T>` annotation into a WIT future.
[Component Model concurrency](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Concurrency.md#streams-and-futures)
provides the boundary mechanics, not these language semantics.

Define the supported surface before advertising it. Stored Promises are a separate
working slice below; constructors, thenables, combinators, callbacks, and detached
work enter through demonstrated consumers. Unsupported forms must be diagnosed,
not silently serialized, fulfilled with dummy values, or mapped to consumed futures.

## Starting Point

These are implementation baselines, not completed outcomes for the new pipeline.

| Evidence | What it establishes | What remains to prove |
| --- | --- | --- |
| Current production compiler, guest runtime, and regression suites | P2 task contracts, guest async/callback behavior, capability APIs, and lifetime regressions to reuse | Equivalent or explicitly revised behavior through the new backend and P3 boundary |
| [Local experiment](experiments/component-async/README.md) and [integration tests](experiments/component-async/tests/native_async.rs) | HIR built directly into WAFFLE SSA; numeric branches/loops/awaits; native byte streams; a real P3 clock import; P2/P3 host coexistence | Production integration, general values, UTF-8 string semantics, exceptions, Promise values, and broader ABI coverage. Its synchronous canonical calls use Wasmtime stack suspension; serial calls and store disposal on cancellation still need the production lifecycle checks below |

## Where the Largest Changes Belong

| Area | Direction of replacement |
| --- | --- |
| Compiler orchestration, rewrites, and dependencies | Preserve binding identity and semantic decisions through HIR; introduce WAFFLE lowering and remove dependence on the old emitter's conventions |
| Guest values and supporting libraries | Establish valid UTF-8 and shared value/call conventions for the primitives, objects, arrays, binary views, and captures that supported tasks need. Verify identity, mutation, escape, and reclamation as each form enters; choose storage from those consumers |
| Exceptions and async execution | Generate control flow deliberately; separate language reactions from host waiting instead of repairing generated Wasm or extending a P2 reactor |
| WIT, ABI, packaging, and linking | Generate against the resolved P3 contracts with existing tools where suitable; replace legacy trampoline, index/table remapping, and memory-patching obligations as their consumers move |
| SDK, capability catalog, conformance, and builds | Describe the actual source semantics and P3 imports; migrate tests and artifacts coherently, with an auditable LLVM-free compiler dependency graph |

## Tracking Work and Completion

Each R-numbered slice has one parent checkbox and identifiable work items.
Tick a work item when its stated outcome is implemented and verified. Tick its
parent only when all required children and the promised outcome are satisfied.

When a probe reveals an unforeseen difficulty, keep the original outcome and
its checkbox open. Add smaller children or linked follow-up slices, stating what
the delivered subset covers and which residual obligation prevents completion.

A required check that failed, was skipped, or lacks a fixture keeps the
relevant item open.

Standing acceptance criteria:

- Execute a representative source task through the new backend and real component
  boundary. Validate the emitted component, inspect its resolved interfaces/imports,
  and use independent host or component bindings to catch ABI mismatches.
- Use Node comparisons for semantics that agree. Give scalar-string differences
  explicit contract expectations and examples; do not disable whole differential suites.
- Check failure, ownership, cancellation, and repeated-allocation behavior where the
  slice introduces them. Distinguish bounded live storage from a fixed-size toy buffer.
- Update SDK declarations, diagnostics, catalog support, and toolchain/artifact contracts
  as applicable.

## Replacement Slices

First establish the WAFFLE basis: R1's executable compiler path, a working
exception/cleanup path from R2, and a real P3 wait from R3. Use primitive values
for these probes. This is enough evidence to begin R4's UTF-8 implementation;
completion of every exception or async case is not a prerequisite.

Implement UTF-8 storage, operations, and text boundaries on that new path.
The contract guides early design, but does not require converting the legacy
runtime, dispatcher, or backend. Investigate frontend assumptions early where
they affect lowering; make semantic changes when the new text path needs them.

Use concrete consumers to shape R5, build shared streams with real I/O, and add
Promise-value support when values escape direct await. R8 expands these paths;
R9 closes the production cutover. String-boundary and async-lifetime checks grow
with their consumers, keeping unresolved obligations open.

### R1 — An LLVM-Free HIR → WAFFLE Compiler Path

- [x] **Compile and run a useful resolved HIR subset through WAFFLE in the production pipeline.**
    - [x] **R1.1:** Bring the experiment's approach into an independently verifiable compiler path. Pin a recent, compatible toolchain and WASI 0.3 WIT inputs through `flake.nix`/`flake.lock`. Make P3 definitions and tools reproducibly available, aligning WIT extraction/resolution with Cargo binding and host async features; keep any still-needed P2 inputs distinct. Change or upgrade dependencies as needed. Retain inspectable HIR/WAFFLE/core/component artifacts and audit compiler dependencies and linked libraries for LLVM/inkwell.
    - [x] **R1.2:** Resolve imports, aliases, built-ins, and shadowing before capability classification loses binding identity. Carry typed operation identity into lowering, preserving receiver/argument evaluation. Cover constructors, methods, defaults, and initialization when exposed.
    - [x] **R1.3:** Prove assignments, branches, loops, function calls, and return/evaluation order using small source tasks with primitive values. Establish the value and call conventions those tasks need without porting the legacy string representation; check SSA and Wasm validity, and reject uncovered HIR explicitly. Reuse only Perry transforms whose outputs survive these probes. Reduce a tooling limitation to a failing case before choosing a dependency fix, alternative lowering, or scoped adapter; keep uncovered forms as residual work.
    - [x] **R1.4:** Rewrite ARCHITECTURE to the final state as if all slices were implemented to the best of the current knowledge.

**Retire:** `perry-codegen-wasm` emission for migrated tasks, name-only call
rewrites, and patches tied solely to that emitter. Keep the legacy route bounded
until R9 accounts for its remaining supported consumers.

*Verification & Implementation Notes (R1 Complete):*
- Pure Rust WAFFLE backend implemented in `src/waffle_backend/` (`mod.rs`, `audit.rs`, `resolve.rs`, `ssa.rs`, `component.rs`) with narrow `pub(crate)` interfaces.
- `audit::audit_no_llvm` verifies 0 LLVM / inkwell dependencies across the compiler graph.
- `resolve_contract` audits and enforces typed intrinsic identities (`waitFor`, `hostDouble`, `readChunk`, `byteAt`), checks illegal local and parameter shadowing, and preserves argument evaluation order.
- `ssa::lower_module` directly constructs WAFFLE basic blocks, SSA values, loop header block parameters, and branch join block parameters, verifying SSA validity with `body.validate()` and `body.verify_reducible()`.
- Component framing embeds the WASI 0.3 monotonic clock adapter (`wasi:clocks/monotonic-clock@0.3.0#wait-for`) with async canonical lower and lift.
- Tested in `tests/waffle_pipeline_test.rs` (15 integration tests) with Wasmtime execution and validation covering branch-local bindings, forward and recursive calls, boolean returns and comparisons, typed await continuations, entry signatures, and async P3 wait suspension alongside the original primitive and diagnostic cases.
- Module initialization is explicitly rejected until it is lowered. Mixed boolean/number comparisons are also rejected rather than emitting invalid Wasm.
- Component `run` signatures follow the resolved entry's parameters and result, including zero arguments, multiple arguments, booleans, and void results. Entries with more than 16 parameters require an unimplemented canonical ABI adapter and are rejected.
- Component import adapters currently support `waitFor` and `hostDouble`. ByteStream inputs and other intrinsics are explicitly rejected during componentization until their adapters are implemented; core-only output remains available through `WaffleCompileOptions { componentize: false, ..Default::default() }`.
- `ARCHITECTURE.md` completely rewritten to reflect the target WAFFLE SSA and WASI 0.3 pipeline.

### R2 — Exceptions and Cleanup Generated from HIR

- [x] **Preserve the promised throwing and cleanup behavior without post-emission repair.**
    - [x] **R2.1:** Choose an exception representation that WAFFLE, the host, and necessary support libraries can express. Prove nested calls, throw/catch, return, and finally ordering with primitive payloads before expanding value and function forms. This first path must work without the UTF-8 implementation; isolate a toolchain gap rather than encode dependence on old Wasm instruction patterns.
    - [x] **R2.2:** Separate language exceptions/rejections, WIT domain errors, host traps, and cancellation. Verify that failures reach the declared guest/host channel and cannot become successful dummy results. Define when an instance is reusable and when it must be discarded. Canonical ABI allocation failure must trap rather than return null for a nonempty allocation; establish the error policy for other resource exhaustion through the consumer that exposes it.
    - [x] **R2.3:** Exercise repeated success and recoverable failure with the values and resources available on the new path. Verify cleanup and bounded live storage; extend coverage to strings after R4, binary values as introduced, and suspension in R3/R6. Keep these later checks open without making them a barrier to the first working backend.

*Verification & Implementation Notes (R2 Complete):*
- Created `src/waffle_backend/exceptions.rs` with narrow `pub(crate)` types: `ExitReason` (`Normal = 0`, `Return = 1`, `Throw = 2`), `UnwindTarget`, `ReturnTarget`, `TryScope`, and `UnwindContext`.
- Intra-module functions lower to a uniform `[Type::I32, Type::F64]` ABI (`0 = Ok`, `1 = Throw`), with caller unpacking and deterministic branch unwinding.
- Implemented SSA try-catch-finally nesting with local variable block-argument threading and three-way finally exit dispatching (`Normal` -> join, `Return` -> outer return target, `Throw` -> outer throw target).
- Infallible exported functions (`run(): number`) emit `Terminator::Unreachable` on uncaught exceptions, triggering a host runtime `Trap` and preventing any throw from masquerading as a successful dummy result (`test_waffle_infallible_uncaught_throw_traps`).
- Fallible WIT exports (`Result<T, E>`) are lifted with Canonical ABI `(memory (core memory $guest "memory"))`, storing discriminant tag (0 = Ok, 1 = Err) and payload to linear memory and returning the retptr `[Type::I32]`.
- Tested instance reuse across repeated success and recoverable domain error invocations (`test_waffle_wit_domain_errors_and_instance_reuse`).
- Tested nested try/catch/finally ordering, returns inside try blocks executing finally clauses, and multi-frame call stack unwinding (`test_waffle_try_catch_finally_ordering`, `test_waffle_multi_frame_unwinding`).
- Linear memory export and automatic resource cleanup (`cleanup_resources()`) run on both normal function returns and unhandled throws.

**Retire:** Exception bytecode scanning/patching and the implicit dispatch-based
exception convention for replaced paths. A language error channel may remain;
its existence does not require preserving today's runtime bridge.

### R3 — P3 Structured Await and Component ABI

- [x] **Run named async source tasks against real P3 operations with correct failure and lifetime behavior.**
    - [x] **R3.1:** Use R1.1's pinned P3 definitions and tooling to generate resolved WIT imports/exports and marshalling with existing Rust/component tools. Add a Nix-backed check that compiles a source task, validates its component, and executes an actually pending P3 clock operation against independently generated host bindings. Begin with primitive values/results; extend to text with R4 and other ABI types through consumers. Verify the resolved WIT, bindings, and enabled host async features agree; isolate demonstrated binding gaps in small adapters. An async export does not by itself require a `future<T>` result.
    - [x] **R3.2:** Select the simplest suspension ABI that passes the task's semantics, using the experiment's stack suspension as a first probe. Make suspension/resumption deliberate in control-flow lowering; if explicit continuations are needed, retain values live across suspension and preserve enclosing cleanup regions. Prove loops/branches and multiple awaits, with rejection entering the guest exception path at the await. Check completed/non-Promise await ordering and host parking without busy polling before eliding language support.
    - [x] **R3.3:** Give pending operations and their ABI buffers, return areas, and borrows clear invocation owners. Reuse bindings for event routing where possible; any guest adapter must distinguish subtask status from stream/future transfer completion and keep language reactions separate from external readiness. Exercise immediate and delayed completion without lost wakeups, duplicate settlement, or resuming a finished invocation; grow this shared mechanism only as consumers require it.
    - [x] **R3.4:** Run the P3 task and an existing P2 component in the same host setup. Record required host features and unsupported ABI/type combinations; leave compiler or binding gaps visible rather than silently falling back to P2 output.
    - [x] **R3.5:** Test cancellation requests, completion races, cleanup, and repeated calls, including release of native handles and protection against stale completion reaching reused state. Record concurrency and instance-reuse limits. Keep pending storage alive until completion or cancellation is acknowledged. Store disposal can establish a scoped lifecycle, but any promised cooperative cleanup remains open until demonstrated; propagate these checks to streams and escaped Promise values as they enter.

*Verification & Implementation Notes (R3 Complete):*
- P3 monotonic clock adapter (`wasi:clocks/monotonic-clock@0.3.0#wait-for`) canonical lower/lift embedded in component framing (`src/waffle_backend/component.rs`).
- `await_expression` in `src/waffle_backend/ssa.rs` lowers:
  1. Immediate / non-Promise values (`await (x * 3)`, `await 5`) without busy polling.
  2. Declared async intrinsics (`waitFor`, `hostDouble`).
  3. Internal async functions (`await step(immediate)`).
- Rejection propagation at await point: when an awaited internal function throws, caller's `err_block` enters the guest exception path via `emit_throw(payload)`, cleanly unwinding to enclosing `catch` and running `finally` blocks with all live locals preserved (`test_waffle_await_rejection_enters_guest_exception_path`).
- Loops and branches with multiple awaits (`test_waffle_async_multiple_awaits_in_loop_and_branch`) compile into verifiable reducible SSA with block parameters.
- Async cancellation via early future drop (`test_waffle_async_cancellation_and_repeated_calls`) proves safe interruption without stale state poisoning across subsequent invocations on the instance.
- Verified in `tests/waffle_pipeline_test.rs` (23 integration tests passing).
- *Modular Architecture & Quality Refactoring (God-Function Elimination & Zero-Redundant-Clone Architecture):*
  - **Tier 1 (Module Declarations):** Extracted `src/waffle_backend/registry.rs` with `ModuleRegistry`, `FunctionInfo`, and explicit `CallingConvention` enum (`Internal`, `ExportedDirect`, `ExportedWitResult`). Pre-declares and registers all module functions and host intrinsics immutably before body lowering begins, eliminating parallel function maps (`func_decls`, `func_is_exported`, `func_return_types`, `intrinsic_funcs`).
  - **Tier 2 (Call and Exit ABI):** Extracted `src/waffle_backend/abi.rs` owning payload encoding/decoding (`encode_payload`, `decode_payload`), retptr memory stores, terminal function returns/throws for all conventions (`emit_function_return`, `emit_function_throw`), and internal call split routing (`emit_internal_call`). Eliminates duplicate ABI encoding and type-erasure conversions across statements and try/finally exits.
  - **Tier 3 (Control-Flow & Join Points):** Extracted `src/waffle_backend/control_flow.rs` providing `JoinPoint` and `create_block_parameters`. Unifies block parameters, binding snapshots, and branch argument construction, avoiding passing full mutable lowerer and eliminating redundant binding map cloning in while loops, continuations, and branches.
  - **Tier 4 (Unwind & Layout Encapsulation):** Refactored `src/waffle_backend/exceptions.rs` to own catch/finally construction, layout encapsulation, and exit dispatch (`TryClauseBlocks`, `emit_finally_dispatcher`, `route_return`, `route_throw`). `TryClauseBlocks` owns parameter layout and restores catch, finally, and join environments directly; unwind and return targets borrow scope local IDs without intermediate vector allocation.
  - **Call & Await Unification:** Unified ordinary and awaited calls under `call_operation`, sharing left-to-right argument evaluation, callee lookup, internal call exception routing, and payload decoding.
  - **Terminal-Exit Plumbing Consolidation:** Consolidated terminal exit emission in `ssa.rs` (`emit_terminal_return`, `emit_terminal_throw`), eliminating repeated context extraction and cleanup closures across return, finally return, and throw.
  - **Ownership & Clone Elimination:** Added `compile_hir_owned` in `src/waffle_backend/mod.rs` to move AST/HIR directly into compilation results; removed unqueried `BTreeSet<String>` cloning in `resolve.rs` shadowing checks.
  - **Result:** `src/waffle_backend/ssa.rs` reduced from 975 lines to 520 lines; `try_statement` reduced from 267 lines to ~65 lines, with fully encapsulated boundaries and zero clippy warnings.

**Retire:** P2 polling/readiness bridges and synchronous task-driving loops for
migrated direct-await consumers. Keep only the shared task/event adapter required
by the selected ABI and guest scheduling required by observable language behavior;
external readiness stays with the host.

### R4 — Valid UTF-8 Values and a Working Text ABI

Begin implementation once R1 and the initial R2/R3 probes establish control flow,
calls, failure handling, and P3 suspension through WAFFLE. Build the string path
once against those conventions; expand or revise them only where a text consumer
demonstrates a gap.

- [ ] **Make the supported string surface obey the scalar contract throughout the new path.**
    - [ ] **R4.1:** Trace decoding, escapes, HIR literals, folding, and intrinsic lowering into the WAFFLE path, locating conversions that can erase invalid input. Establish a compact operation matrix recording index units, coercion, boundary behavior, and deliberate Node differences. Probe ASCII, BMP/non-BMP text, combining sequences, empty strings, paired escapes, and malformed inputs. Fix required shared frontend/transform behavior at that boundary, keeping legacy runtime conversion outside this slice.
    - [ ] **R4.2:** Deliver a source task through WAFFLE using scalar length/index/slice/search and a UTF-8 WIT string round trip. Add simple valid-text storage and ownership to the established value/call/ABI conventions, reusing Rust facilities where suitable. Verify negative/out-of-range bounds and search-position reuse; compare literals with runtime expressions and representative function/class paths. Keep ABI pointers/lengths in the required units and test exact bytes for empty, permitted embedded-NUL, and multibyte text.
    - [ ] **R4.3:** Migrate the matrix's remaining operations through combined tasks: scalar `charAt`/code-point access, empty-separator splitting and iteration; concatenation/templates/joins/replacement/conversion, including nested values; comparison/sorting; case/whitespace helpers; JSON values and keys; regex positions. Test ordering that differs from UTF-16, case mappings that change length, and paired/unpaired JSON escapes. Document supported regex flags/classes and UTF-16-specific API decisions; split distinct gaps without making a full ECMAScript regex engine a prerequisite.
    - [ ] **R4.4:** Audit text import/export, filesystem, HTTP, environment, arguments, and logging as each boundary is introduced. Preserve arbitrary binary bytes and API-specific restrictions; reject invalid text through the proper error channel before dependent side effects. Verify literal/global/returned/retained lifetimes and repeated allocating calls, cleanup, and recoverable failures for ASCII and multibyte strings. Require bounded live storage and string-only tasks without unrelated imports; measure before adding caches or more elaborate storage.

**Retire:** UTF-16/WTF-8 runtime string storage, surrogate-half operations, lossy
conversions that hide invalid input, and compiler shortcuts that preserve the old
contract as their consumers migrate; remove obsolete legacy code rather than
converting it first. Paired-escape decoding remains an input codec, not an internal string mode.
Use contract-specific Unicode expectations while retaining matching Node comparisons.

### R5 — Typed Capability Lowering with a Small Rust Trait

- [ ] **Lower resolved capability operations through a shared, consumer-shaped compiler contract.**
    - [ ] **R5.1:** Derive the trait from two concrete consumers, such as clocks and the first filesystem operation. Group typed operations by responsibility and carry the operands, result/error behavior, and imports their lowering needs. Choose methods and context from those implementations; defer registration, dynamic loading, packages, and feature combinations until a consumer demonstrates a need.
    - [ ] **R5.2:** Keep language values, exceptions, async suspension, stream transfers, and ABI ownership in shared lowering/support code. Capability implementations use these mechanisms and do not own separate schedulers, Promise engines, or stream registries. Verify a mixed-capability source task to expose misplaced responsibilities early.
    - [ ] **R5.3:** Verify bound aliases, shadowed built-ins, unrelated member names, dynamic forms that are exposed, and argument side effects. Inspect pure and mixed tasks' component imports and instantiate with only the requested interfaces. Retain ABI, indirect-call/global references, and initialization dependencies correctly; add finer pruning only when measured output needs it.

**Retire:** String capability markers, generic `mem_call` capability routing,
and dispatcher-combination specialization for replaced operations. Conservative
reachability remains useful where support code needs it; a feature-matrix runtime
or replacement plugin architecture is not the goal.

### R6 — Minimal Promise Values for a Real Consumer

- [ ] **Support a task that starts work, stores its Promise, and awaits it more than once.**
    - [ ] **R6.1:** Establish the retained-result/rejection and identity contract with a real consumer, verifying the caller continues while the operation remains pending. Consume a P3 result once and retain the language outcome for later awaits where necessary. Separate observation of that outcome from authority to settle it. Choose the smallest representation these cases require, measuring per-await/settlement allocations before adding optimizations.
    - [ ] **R6.2:** Prove eager execution up to suspension, single settlement, repeated awaits, multiple observers, and supported reaction ordering with trace-based tests. Lower away Promise objects only where equivalence holds. Extend constructors, adoption, then/catch/finally, or combinators through required consumers; diagnose unimplemented forms. Track additional API coverage separately from this stored/repeated-await slice, with existing advertised obligations retained in the migration matrix.
    - [ ] **R6.3:** Define owners for pending operations, settled results, observers, and escaped captures. Cancelling one observer must follow the stated policy for other observers, not automatically destroy their shared operation. Verify rejection, abandonment, cancellation races, and repeated allocating cycles release roots and reach bounded live storage. Any exposed callback needs a tested invocation/capture ABI before a timer or executor relies on it.

**Retire:** Perry-state-machine-to-guest-cell adaptation and the old continuation
queue/runtime where the replacement satisfies its consumers. Retained outcomes
and observable reaction scheduling may still need small guest support; neither is
supplied by the host future's one-result transport.

### R7 — Shared Native Streams through Real I/O

- [ ] **Process and forward data incrementally through P3/native stream boundaries with bounded storage.**
    - [ ] **R7.1:** Extend the fixed-buffer experiment through an actual P3 I/O capability, starting with a file or HTTP workload. Exercise read and write/forwarding paths against resolved WIT and a real producer/consumer; choose batch sizes and adapter ownership from that workload. Check stream types and ownership against independent bindings, adding a composed-component probe where host fixtures leave a contract untested.
    - [ ] **R7.2:** Share byte buffers, views, transfer and completion handling across capabilities. Prove exact binary bytes, partial transfers, empty chunks, EOF, and separate failure/completion channels; EOF alone need not mean success. Text decoding must preserve scalars split across chunks and reject malformed or unfinished sequences. A returned stream/future may outlive result delivery: retain its producer, pending buffers, and native handles until completion or acknowledged cancellation. Split lifecycle gaps into follow-ups rather than treating export return as universal cleanup.
    - [ ] **R7.3:** Use delayed producers and slow/abandoned consumers to verify backpressure, cancellation, early return, error cleanup, and ownership of both stream ends. Check input/output larger than guest memory and repeated calls for bounded live storage. Promised overlapping calls also need per-call isolation; otherwise record the serial limit. Split concurrency work out if it blocks a smaller consumer, keeping any original concurrency promise open.

**Retire:** P2 stream-resource polling, readiness subscriptions, and bespoke
`guest_stream_step`/body-forwarding protocols for migrated consumers. Keep any
source-level stream wrapper limited to its advertised semantics; Web Streams,
async iteration, and zero-copy claims need their own consumer evidence.

### R8 — Migrate the Advertised Capability Surface

- [ ] **Make currently promised capability tasks work through typed lowering and the new contracts.**
    - [ ] **R8.1:** Migrate clocks, randomness, environment/arguments, and required timer APIs in small combined tasks. Preserve time units, random-fill view identity/quotas, context coercion/mutation, and promised callback ordering/lifetimes. Validate inputs before side effects and test cancellation without assuming rollback; recurring work must not grow retained storage without bound. Use controlled host input for nondeterministic behavior.
    - [ ] **R8.2:** Migrate filesystem consumers with preopen confinement, exact binary I/O, and valid UTF-8 text boundaries. Preserve options and argument effects, honoring or rejecting permissions/flags/encodings before I/O. Verify denial, invalid text/options, resource cleanup, repeated calls, and unrelated user functions through the public component path. Split metadata or other gaps according to actual consumers without closing the filesystem promise early.
    - [ ] **R8.3:** Migrate outgoing HTTP and incoming handlers using P3 bindings and R7's shared body transfers. Preserve the supported method/header/status/body contracts and error channels. Verify delayed I/O, concurrent requests when advertised, cancellation before/after response return, abandoned bodies, and bounded retained resources; split buffered and streaming subsets when their implementation or lifecycle needs differ, leaving the parent open until both promised paths work.

**Retire:** Each migrated capability's P2 bindings, name-based rewrites, and
capability-owned runtime/readiness adapter once its consumers are covered. Reuse
existing fixtures and behavior tests; do not require a fresh test file per variant
or expand into new APIs to declare migration complete.

### R9 — Production Cutover and Retirement

- [ ] **Make HIR → WAFFLE → P3 the production pipeline and remove its superseded dependencies and machinery.**
    - [ ] **R9.1:** Reconcile the migration matrix established at the start: every advertised source/API/WIT consumer has a verified new path, an explicit semantic change, or a still-open gap. Resolve required gaps before removing the legacy route. A smaller successful subset is progress, not completion of the original production replacement.
    - [ ] **R9.2:** Remove `perry-codegen-wasm`, emitter-specific Wasm patches, unused runtime/dispatcher/Promise/stream code, and linker/table/memory workarounds with no remaining consumers. Simplify ABI/linking around the final support modules; audit actual references and complete ABI roots before deleting helpers or replacing custom merging with existing tools.
    - [ ] **R9.3:** Finish production packaging on the P3 Nix build path established in R1/R3, aligning WIT, SDK declarations, catalog support, Unicode differential expectations, fixtures, bundled libraries, and generated artifacts. Audit compiler and library paths for surviving UTF-16/WTF-8 storage, surrogate-half operations, and implicit lossy conversion; account for any legitimate input codec. Run the new production compiler/component/host, failure/cancellation, ownership/memory, and capability-import suites. Keep matching Node comparisons and P2/P3 host coexistence checks; verify the compiler's final dependency/link graph remains LLVM-free.
    - [ ] **R9.4:** Record component size, memory, and representative string/I/O throughput for the final path against useful earlier baselines. Address demonstrated regressions and publish the supported surface and limitations. Rebuild incompatible artifacts coherently; do not require speculative optimization work to close a correct, measured cutover.

**Retire:** The legacy compilation route and compatibility scaffolding that only
kept its implementation alive. Keep useful regression fixtures and any small
language/ABI support still justified by the established contracts.

## Complexity Deferred Until a Consumer Requires It

| Area | Initial approach | Evidence needed to expand |
| --- | --- | --- |
| Complete JavaScript/Node compatibility | Explicit source/API subsets, scalar UTF-8 semantics, matching differential cases | A consumer needing another Promise, string/regex, Date, Buffer, or event API |
| New networking breadth | Migrate existing HTTP; a first TCP client uses the shared P3 mechanisms when needed | A concrete socket workload; UDP/TLS/general event APIs are not cutover prerequisites |
| Value/string storage sophistication | Simple typed support and lifetimes that pass escape, cycle, and memory probes | A supported graph or measured performance issue the simpler representation cannot handle |
| Custom component tooling | Existing WIT/binding/encoding tools plus isolated, tested gaps | A working ABI consumer that those tools cannot express |
| Plugin systems, feature matrices, separate runtime packages | One small capability-lowering trait and selected imports/support | Demonstrated distribution constraints, not a growing list of capability names |
| Aggressive optimization and zero-copy designs | Correct control flow, conservative dependency selection, bounded transfers | Measured component size, copying, memory, or throughput bottlenecks |

Correct exceptions, Unicode semantics, ownership, cancellation behavior, and bounded
live memory remain acceptance obligations for every slice that promises them.
They are not deferred merely because a prototype works on the happy path.
