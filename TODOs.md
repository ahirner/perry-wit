# Perry-WIT Roadmap: Perry HIR → WAFFLE → WASI 0.3

Transform perry-wit into a compiler for statically typed TypeScript with a dedicated
Perry HIR → WAFFLE backend and WASI 0.3 (P3) component interfaces. Keep LLVM out
of perry-wit's dependency and link graph. Reuse Perry's frontend and suitable
transforms, Rust libraries, and component binding tools; replace the implementation
choices that obstruct this path rather than preserving the current guest runtime
as a requirement.

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
documented typed argument and out-of-range behavior. Arbitrary bytes remain
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

Keep correctness gaps within the supported static subset open. Deferred
compatibility work does not block a parent or become required merely because
the legacy implementation or its tests covered it.

A required check that failed, was skipped, or lacks a fixture keeps the
relevant item open.

Standing acceptance criteria:

- Execute a representative source task through the new backend and real component
  boundary. Validate the emitted component, inspect its resolved interfaces/imports,
  and use independent host or component bindings to catch ABI mismatches.
- Use Node comparisons for supported semantics that agree. Give scalar-string
  differences explicit expectations. Replace excluded dynamic cases with diagnostic
  tests where needed; retain relevant regressions rather than disabling whole suites.
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
R9 closes the production cutover for the required static consumers. Boundary
and lifetime checks follow those consumers; deferred APIs do not block completion.

### R1 — An LLVM-Free HIR → WAFFLE Compiler Path

- [x] **Compile and run a useful resolved HIR subset through WAFFLE in the production pipeline.**
    - [x] **R1.1:** Bring the experiment's approach into an independently verifiable compiler path. Pin a recent, compatible toolchain and WASI 0.3 WIT inputs through `flake.nix`/`flake.lock`. Make P3 definitions and tools reproducibly available, aligning WIT extraction/resolution with Cargo binding and host async features; keep any still-needed P2 inputs distinct. Change or upgrade dependencies as needed. Retain inspectable HIR/WAFFLE/core/component artifacts and audit compiler dependencies and linked libraries for LLVM/inkwell.
      - [x] **R1.1a:** Repair the empty P3 WIT output discovered during R5 preparation. Extract the six released packages from the pinned `proposals/*/wit` directories, fail on missing inputs, expose `packages.wasi-p3-wit`, and resolve the CLI and HTTP dependency graph in `checks.wasi-p3-wit`. Verified with `nix build .#checks.aarch64-darwin.wasi-p3-wit --no-link`.
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
- Tested in `tests/waffle_pipeline_test.rs` (29 integration tests across R1–R3) with Wasmtime execution and validation covering branch-local bindings, forward and recursive calls, boolean returns and comparisons, typed await continuations, entry signatures, and async P3 wait suspension alongside the original primitive and diagnostic cases.
- Module initialization is explicitly rejected until it is lowered. Mixed boolean/number comparisons are also rejected rather than emitting invalid Wasm.
- Component `run` signatures follow the resolved entry's parameters and result, including zero arguments, multiple arguments, booleans, and void results. Entries with more than 16 parameters require an unimplemented canonical ABI adapter and are rejected.
- Component import adapters now support clocks, random numbers, `hostDouble`, and the R7.1 native ByteStream input contract. Other intrinsics are explicitly rejected until their adapters are implemented; core-only output remains available through `WaffleCompileOptions { componentize: false, ..Default::default() }`.
- `ARCHITECTURE.md` completely rewritten to reflect the target WAFFLE SSA and WASI 0.3 pipeline.

### R2 — Exceptions and Cleanup Generated from HIR

- [x] **Preserve the promised throwing and cleanup behavior without post-emission repair.**
    - [x] **R2.1:** Choose an exception representation that WAFFLE, the host, and necessary support libraries can express. Prove nested calls, throw/catch, return, and finally ordering with primitive payloads before expanding value and function forms. This first path must work without the UTF-8 implementation; isolate a toolchain gap rather than encode dependence on old Wasm instruction patterns.
    - [x] **R2.2:** Separate language exceptions/rejections, WIT domain errors, host traps, and cancellation. Verify that failures reach the declared guest/host channel and cannot become successful dummy results. Define when an instance is reusable and when it must be discarded. Canonical ABI allocation failure must trap rather than return null for a nonempty allocation; establish the error policy for other resource exhaustion through the consumer that exposes it.
    - [x] **R2.3:** Exercise repeated success and recoverable failure with the values and resources available on the new path. Verify cleanup and bounded live storage; extend coverage to strings after R4, binary values as introduced, and suspension in R3/R6. Keep these later checks open without making them a barrier to the first working backend.

*Verification & Implementation Notes (R2 Complete):*
- Thrown payloads currently support numbers only. Boolean throws are rejected in both core-only and component output until the exception ABI carries primitive type tags; this prevents catches from observing a boolean as a number.
- Created `src/waffle_backend/exceptions.rs` with narrow `pub(crate)` types: `ExitReason` (`Normal = 0`, `Return = 1`, `Throw = 2`), `UnwindTarget`, `ReturnTarget`, `TryScope`, and `UnwindContext`.
- All source functions, including exported functions, lower to a uniform `[Type::I32, Type::F64]` guest ABI (`0 = Ok`, `1 = Throw`), with caller unpacking and deterministic branch unwinding. Separate host export wrappers convert completions to direct values, traps, or WIT results, so guest calls preserve catch/finally semantics regardless of export visibility.
- Implemented SSA try-catch-finally nesting with local variable block-argument threading and three-way finally exit dispatching (`Normal` -> join, `Return` -> outer return target, `Throw` -> outer throw target).
- Infallible exported functions (`run(): number`) emit `Terminator::Unreachable` on uncaught exceptions, triggering a host runtime `Trap` and preventing any throw from masquerading as a successful dummy result (`test_waffle_infallible_uncaught_throw_traps`).
- Fallible WIT exports (`Result<T, E>`) are lifted with Canonical ABI `(memory (core memory $guest "memory"))`, storing discriminant tag (0 = Ok, 1 = Err) and payload to linear memory and returning the retptr `[Type::I32]`.
- Supported WIT results have number or boolean success payloads and numeric error payloads. Boolean successes use byte stores at the union's aligned payload offset; nonnumeric error types and other payload layouts are rejected before emission.
- Tested instance reuse across repeated success and recoverable domain error invocations (`test_waffle_wit_domain_errors_and_instance_reuse`).
- Tested nested try/catch/finally ordering, returns inside try blocks executing finally clauses, and multi-frame call stack unwinding (`test_waffle_try_catch_finally_ordering`, `test_waffle_multi_frame_unwinding`).
- Linear memory export and automatic resource cleanup (`cleanup_resources()`) run on both normal function returns and unhandled throws.

**Retire:** Exception bytecode scanning/patching and the implicit dispatch-based
exception convention for replaced paths. A language error channel may remain;
its existence does not require preserving today's runtime bridge.

### R3 — P3 Structured Await and Component ABI

- [ ] **Run named async source tasks against real P3 operations with correct failure and lifetime behavior.**
    - [x] **R3.1:** Use R1.1's pinned P3 definitions and tooling to generate resolved WIT imports/exports and marshalling with existing Rust/component tools. Add a Nix-backed check that compiles a source task, validates its component, and executes an actually pending P3 clock operation against independently generated host bindings. Begin with primitive values/results; extend to text with R4 and other ABI types through consumers. Verify the resolved WIT, bindings, and enabled host async features agree; isolate demonstrated binding gaps in small adapters. An async export does not by itself require a `future<T>` result.
    - [x] **R3.2:** Select the simplest suspension ABI that passes the task's semantics, using the experiment's stack suspension as a first probe. Make suspension/resumption deliberate in control-flow lowering; if explicit continuations are needed, retain values live across suspension and preserve enclosing cleanup regions. Prove loops/branches and multiple awaits, with rejection entering the guest exception path at the await. Check completed/non-Promise await ordering and host parking without busy polling before eliding language support.
    - [x] **R3.3:** Give pending operations and their ABI buffers, return areas, and borrows clear invocation owners. Reuse bindings for event routing where possible; any guest adapter must distinguish subtask status from stream/future transfer completion and keep language reactions separate from external readiness. Exercise immediate and delayed completion without lost wakeups, duplicate settlement, or resuming a finished invocation; grow this shared mechanism only as consumers require it.
    - [x] **R3.4:** Run the P3 task and an existing P2 component in the same host setup. Record required host features and unsupported ABI/type combinations; leave compiler or binding gaps visible rather than silently falling back to P2 output.
    - [ ] **R3.5:** Test cancellation requests, completion races, cleanup, and repeated calls, including release of native handles and protection against stale completion reaching reused state. Record concurrency and instance-reuse limits. Keep pending storage alive until completion or cancellation is acknowledged. Store disposal can establish a scoped lifecycle, but any promised cooperative cleanup remains open until demonstrated; propagate these checks to streams and escaped Promise values as they enter.

*Verified structured-await behavior; cooperative cancellation remains open:*
- P3 monotonic clock adapter (`wasi:clocks/monotonic-clock@0.3.0#wait-for`) canonical lower/lift embedded in component framing (`src/waffle_backend/component.rs`).
- `await_expression` in `src/waffle_backend/ssa.rs` lowers:
  1. Immediate / non-Promise values (`await (x * 3)`, `await 5`) without busy polling.
  2. Declared async intrinsics (`waitFor`, `hostDouble`).
  3. Internal async functions (`await step(immediate)`).
- Rejection propagation at await point: when an awaited internal function throws, caller's `err_block` enters the guest exception path via `emit_throw(payload)`, cleanly unwinding to enclosing `catch` and running `finally` blocks with all live locals preserved (`test_waffle_await_rejection_enters_guest_exception_path`).
- Loops and branches with multiple awaits (`test_waffle_async_multiple_awaits_in_loop_and_branch`) compile into verifiable reducible SSA with block parameters.
- `tests/p3_cancellation_test.rs` verifies native subtask cancellation acknowledgment, completion races, and callback cancellation propagated through a composed application component. All 900 calls release native owners and runtime state. Wasmtime 49 requires immediate callback results to be delivered on the next callback turn to avoid retained subtask state. Production source operations still need retained cancellable handles and callback entry; these ABI probes do not complete R3.5.
- Early future-drop tests cover their scoped host disposal behavior. They do not establish acknowledged cooperative cancellation, which remains required under R3.5/R6.3.
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

- [x] **Make the supported string surface obey the scalar contract throughout the new path.**
    - [x] **R4.1:** Trace decoding, escapes, HIR literals, folding, and intrinsic lowering into the WAFFLE path, locating conversions that can erase invalid input. Establish a compact operation matrix recording index units, coercion, boundary behavior, and deliberate Node differences. Probe ASCII, BMP/non-BMP text, combining sequences, empty strings, paired escapes, and malformed inputs. Fix required shared frontend/transform behavior at that boundary, keeping legacy runtime conversion outside this slice.
      - **Tracing & Lossy Erasure Identification:** Located lossy conversion points where invalid input was previously erased: template literal unescaping in `perry-hir` silently converting lone surrogates to `\u{FFFD}`, and WTF-8 literals emitting `Expr::WtfString`.
      - **Compact Operation Matrix:** Established typed `TextContractMatrix` in `src/waffle_backend/text_contract.rs` defining exact indexing units (`ScalarValue`, `Utf16CodeUnit`, `Byte`), operation statuses (`SupportedScalar`, `DisallowedUtf16`, `SupportedBoundary`), coercion rules, boundary behavior, and deliberate Node differences for 15 core operations (`length`, `index_access`, `charAt`, `codePointAt`, `charCodeAt`, `fromCodePoint`, `fromCharCode`, `slice`, `indexOf`, `split`, `concat`, `comparison`, `template_literal`, `surrogate_escapes`, `canonical_abi_string`).
      - `validate_source_text` visits parsed string literals and template quasis before HIR lowering replaces invalid text.
        Nested interpolations retain their boundaries; comments and regular expressions are excluded.
        `validate_hir_text` rejects `Expr::WtfString` and disallowed UTF-16 operations; `validate_utf8_boundary` checks input bytes.
      - **Probe & Acceptance Verification:** Added comprehensive acceptance suites in `tests/utf8_probe_test.rs` and `tests/text_contract_test.rs` verifying ASCII, BMP, non-BMP, combining sequences, empty strings, embedded NULs, paired escapes, and rejection of malformed inputs. Verified all 8 contract tests and 29 WAFFLE pipeline integration tests pass.
    - [x] **R4.2:** Deliver a source task through WAFFLE using scalar length/index/slice/search and a UTF-8 WIT string round trip. Add simple valid-text storage and ownership to the established value/call/ABI conventions, reusing Rust facilities where suitable. Verify negative/out-of-range bounds and search-position reuse; compare literals with runtime expressions and representative function/class paths. Keep ABI pointers/lengths in the required units and test exact bytes for empty, permitted embedded-NUL, and multibyte text.
      - String descriptors contain `[ptr: i32, byte_len: i32, scalar_len: i32]` with four-byte alignment.
        Static memory reserves enough pages for the complete literal pool.
        The bump allocator grows memory before returning an allocation and traps on overflow or failed growth.
        Repeated calls retain allocated storage until the instance is dropped; reclamation remains part of R4.4.
      - `strings/` separates allocation, descriptor construction, canonical input handling, position normalization,
        slicing, search, concatenation, and comparison.
        Concatenation uses `memory.copy`; slices retain the original byte storage.
      - Canonical string parameters flatten to `(i32, i32)`; entries exceeding 16 flattened parameters are diagnosed.
        Direct strings and `Result<string, number>` use UTF-8 byte lengths at the component boundary.
        Promise-wrapped results use the same canonical options as their resolved types.
      - `.length`, indexing, `charAt`, `slice`, and `indexOf` use scalar positions.
        Numeric method positions truncate toward zero, map NaN to zero, and clamp infinities before integer conversion.
        `slice()` and `charAt()` default to zero; `slice` defaults its end to the string length.
        Unsupported argument types and arities are diagnosed.
      - Bracket access returns `undefined` for nonintegral, negative, or out-of-range numeric indices.
        That value survives locals and `await`, remains falsy, and compares unequal to empty strings and other primitives.
        String methods and declared string argument/return boundaries trap if given `undefined`.
        `charAt` returns an empty string outside its bounds.
      - String truthiness tests length; strict equality distinguishes strings, numbers, booleans, and `undefined`.
        Concatenation and template interpolation require known string operands.
        Other conversions, mixed loose equality/ordering, and ordering string-or-undefined values are diagnosed.
      - `tests/waffle_string_test.rs` executes component round trips for empty, ASCII, NUL, and multibyte strings,
        including awaited values, missing indices, numeric bounds, large literals, and repeated allocating calls.
        Allocator probes also check failed growth, address overflow, alignment, and preserved realloc contents.
    - [x] **R4.2a — Establish reusable guest helpers and their build/link foundation before expanding the string surface.**
      - Start with one substantial existing operation, such as scalar slice scanning or search, implemented in ordinary Rust over borrowed UTF-8 bytes and returning offsets/status. Preserve R4.2's descriptor layout, scalar positions, argument normalization, coercion/error rules, and regression tests. Keep simple descriptor loads and truthiness checks in WAFFLE; separate pure algorithms from allocation and other effects. Remove the superseded algorithm once the helper passes the same tests.
      - Integrate helper production into the normal Cargo/Nix build: compile helper sources with the pinned Rust toolchain and `wasm32-unknown-unknown` target, validate the resulting Wasm, and embed artifacts from `OUT_DIR`. Track sources, manifests, lockfiles, and relevant build settings for rebuilds; isolate helper build outputs to avoid recursive workspace builds. Include helper dependencies in Nix vendoring and source inputs so clean, sandboxed builds need neither network downloads nor a manually populated `target/`. Keep these artifacts independent of the legacy `guest_runtime.wasm`. Task compilation uses the embedded artifacts without invoking Rust/LLVM; document the distinction between producing helpers with the Rust toolchain and linking LLVM into the deployed compiler.
      - Define a small typed helper ABI using explicit scalars, pointers, lengths, and status/results rather than Rust `String`/`str` layouts. Share the application's memory, allocator, ownership, and exception conventions; record borrowed/result lifetimes and map expected failures into the guest error path. Use this same foundation for later text algorithms and consumers such as Date, without creating a second heap, dispatcher, or capability framework.
      - Resolve symbolic helper calls through one shared linking path, retaining only requested functions and their transitive dependencies. Account for required data, globals, tables, initialization, and canonical ABI roots; validate signatures and relocations with existing Wasm tooling where suitable. Repeated uses call one shared implementation. Keep unrelated helper families absent, and measure before adding finer pruning or aggressive inlining.
      - Verify clean Cargo/Nix builds and helper-source rebuilds, then execute the existing string regressions through the linked path. Compare numeric-only, single-operation, and repeated-operation tasks: inspect helper inclusion and sharing, record component size and representative execution cost, and exercise allocation boundaries for allocating helpers. Preserve open reclamation obligations in R4.4. Establish these checks here and extend them as each helper family lands rather than waiting for R9.
      - **Verification & Resolution:**
        - **Selective Helper Linking:** `scan_module_string_requirements` and `link_helpers` only link helpers demanded by the AST. Numeric-only modules link zero helpers and have zero helper imports (`tests/waffle_string_test.rs::test_unused_helpers_are_not_embedded_for_numeric_and_simple_tasks`).
        - **Zero Compiler Memory Leaks:** `link.rs` parses functions borrowing directly from input bytes (`&[u8]`) without `.leak()` static allocations. Verified by measuring net heap memory across 200 repeated compilations (`test_string_boundary_audit_and_bounded_storage`).
        - **Complete Relocation Engine:** Dynamic stack region (`64KB`) is placed above relocated helper data segments (`placement.memory_base`), element segments are appended to the table (`placement.table_base`), memory pages expand to cover stack and heap, and helper initialization routines (`start`, `__wasm_apply_data_relocs`, `__wasm_call_ctors`, `_initialize`) are invoked via synthetic start.
        - **Robust Build Test:** `tests/build_artifact_test.rs` compiles `build.rs` directly with `rustc` supplying `wasmparser`, executes with helper fixtures, validates generated wasm, and checks `cargo:rerun-if-changed` lines.
    - [x] **R4.3:** Migrate the matrix's remaining operations through combined tasks: scalar `codePointAt` / `fromCodePoint`, empty-separator and character `split` into string arrays, and scalar iteration; string templates and joins (`join`); case conversions (`toLowerCase`, `toUpperCase`) with length-changing Unicode casing; JSON text parsing and serialization; regex search positions. Build on R4.2a's helper foundation and R4.2's 12-byte descriptor model for string array representations in linear memory. Prefer evaluated Rust implementations for substantial algorithms, adapting byte offsets, coercion, and error behavior to the scalar contract. Test ordering differences from UTF-16 and reject unpaired JSON escapes.
      - Reopened during R6 preparation: the original operation implementations did not establish scalar iteration, executable JSON parsing/serialization, or regex search positions.
      - [x] **R4.3a:** Lower scalar string iteration through executable source `for…of` loops. The private iteration view retains the original immutable descriptor, evaluates the input once, and yields full scalars without an intermediate character array. Shared `for`/`while` control flow preserves numeric update effects and routes break/continue through only the crossed finally clauses. Node comparisons cover nested loops, snapshotting, abrupt exits, and cleanup overrides; independent P3 clock bindings prove suspension within iteration. Shared expression traversal retains helper/literal dependencies and validates text inside the new loop forms.
        - Verified 37 string, 36 pipeline, 14 Promise, and 12 text/probe regressions, Clippy, and local aarch64-darwin Nix flake checks.
      - [x] **R4.3b:** Deliver executable JSON value-tree parsing/serialization, including malformed input and unpaired-escape rejection, through source tasks and the component boundary. Reuse this codec with consumer-specific validation before treating external JSON as a typed record; assertions and `JSON.parse<T>` do not validate input. General reflection and schema frameworks are deferred under D7.
        - [x] **R4.3b.i:** Implement an allocation-free Rust codec for caller-owned storage. Parsing measures exact storage, writes typed nodes with explicit owner addresses, preserves binary64 values, decodes paired escapes, and rejects malformed text, unpaired escapes, and nesting beyond 128 levels. Serialization preserves ECMAScript numeric formatting, duplicate-key replacement, integer-key ordering, and arbitrary valid Unicode. Host tests compare hundreds of number/Unicode/object cases with Node and verify range/capacity failures. Source validation preserves shadowed JSON bindings and diagnoses unsupported arguments before Perry can discard their effects.
          - Verified six new codec/source tests, 97 compiler/string/Promise regressions, Clippy, and local aarch64-darwin Nix checks. The codec package adds no unsafe code; executable source JSON is tracked in R4.3b.ii.
        - [x] **R4.3b.ii:** Wire the codec into source JSON expressions and the component boundary, route parse failures through catch/finally, and verify capped allocating loops, imports, and suspension.
          - Parsed graphs use shared tagged objects and mixed arrays, preserving aliases, indexed mutation, holes, deletion, helper calls, and retained Promises. Serialization covers primitive/plain-object/array/Date values, undefined rules, and numeric failures through catch/finally and WIT errors. Node comparisons, independent environment snapshots, pending/repeated awaits, disposal, 150 KB inputs, and capped allocating loops pass. Verified all 433 Cargo tests, strict Clippy and SDK declarations, formatting, and all eight local aarch64-darwin Nix checks. Pure JSON tasks have no host imports; README, SDK notes, and catalog specify unsupported options and guest-internal graph boundaries.
          - [x] **R4.3b.ii.a:** Compile the codec as a relocatable Wasm helper with the approved, checked Rust guest-memory adapter. Validate memory ranges, disjoint borrows, strict UTF-8, graph bounds, and capacity before output writes. Reserve helper data and stack using the same measured layout as the linker. Raw and linked execution, deep nesting, repeated reuse, Node round trips, 422 Cargo tests, native/Wasm Clippy, formatting, and all eight local aarch64-darwin Nix checks pass. Source integration is tracked in R4.3b.ii.
      - [x] **R4.3c:** Deliver regex search positions in scalar units and verify that returned positions work with indexing/slicing. Literal `string.search` patterns compile through Rust `regex-automata` into immutable reverse DFA tables. Overlapping reverse matches establish the leftmost start, converted from byte position to a scalar index; matching takes linear time and allocates no guest storage.
        - Node comparisons cover scalar-adjusted positions for regular groups, alternation, repetition, character classes, Unicode escapes, anchors, ASCII word boundaries, whitespace, and empty/overlapping matches. Returned positions work with scalar slices and linked casing/split/join helpers across a real P3 wait. Twenty calls perform 400,000 searches under a 64 KiB cap with no capability imports.
        - The documented slice accepts regex literals with `u`/`s` flags. Constructors, stored/dynamic RegExp values, other flags, lookaround, backreferences, and Unicode property escapes are diagnosed. Source binding checks preserve shadowed `RegExp` functions and reject constructors before frontend folding can discard argument effects. Invalid scalar escapes and oversized automata are compile errors.
        - Verified all 105 affected compiler/text/Promise regressions, including 330 Node regex comparisons, Clippy, and local aarch64-darwin Nix flake checks.
      - Implemented scalar `codePointAt` / `fromCodePoint` handling BMP, non-BMP (surrogate pairs / astral planes), and empty / boundary cases.
        `codePointAt` normalizes numeric positions and retains number-or-undefined identity in guest expressions.
        Invalid `fromCodePoint` inputs enter catch/finally through the completion ABI.
        That ABI carries the rejected numeric input; JavaScript Error objects remain unsupported.
      - Implemented Unicode case conversion (`toLowerCase`, `toUpperCase`) supporting full length-changing transformations (e.g. German sharp S `ß` -> `SS`, ligature expansion) and multibyte scripts (Greek, Cyrillic, emoji).
        Mapping uses Rust's Unicode case iterators and Unicode 17 casing-context properties for final sigma.
        `scripts/generate_unicode_casing.py` regenerates the checked-in property ranges from `DerivedCoreProperties.txt`.
        The helper measures exact output bytes before allocating, and helper linking retains data relocation initializers.
      - Implemented string `split` and `join` with array descriptors in guest linear memory, supporting empty separators, arbitrary delimiters, and default joining.
        Split-array access returns undefined for invalid numeric indices; join requires a string-array receiver.
        Split and join sizes are checked before narrowing to the allocator's wasm32 size argument.
      - Emitted helper routines in `src/helpers/text.rs` and compiled into embedded `text.wasm` cdylib helper library.
    - [x] **R4.4:** Audit text import/export, filesystem, HTTP, environment, arguments, and logging as each boundary is introduced. Preserve arbitrary binary bytes and API-specific restrictions; reject invalid text through the proper error channel before dependent side effects. Verify literal/global/returned/retained lifetimes and repeated allocating calls, cleanup, and recoverable failures for ASCII and multibyte strings. Require bounded live storage and string-only tasks without unrelated imports; measure before adding caches or more elaborate storage.
        - Validated UTF-8 boundary checking via `text_contract::validate_utf8_boundary` ensuring invalid input text is rejected.
        - Reopened during R6 preparation: the prior test measured pages on an uncalled core instance and did not prove reclamation. The allocator grew monotonically across calls.
        - [x] **R4.4a:** Reclaim serial invocation arenas through canonical post-return after the host copies each result. The strengthened test first reproduced exhaustion, then passed 200 large multibyte casing/split/join calls under a 512 KiB limit. Raw canonical calls verify output bytes before cleanup, allocator reuse, and a stable page count after the first call. Separate capped tests cover recursive guest calls, void/scalar/string exports, and alternating recoverable failures. All 70 pipeline/string tests and Clippy pass.
        - [ ] **R4.4b:** Reclaim dead temporary values within long-running invocations and retain supported Promise and native-transfer owners until completion. Finish boundary audits for required consumers; a serial invocation arena alone does not satisfy those lifetimes. Returned P3 streams must retain transfer owners through completion or cancellation; general detached work remains D6.
          - [x] **R4.4b.i:** Reclaim text and Promise temporaries at source loop backedges. A non-moving typed tracer follows source root frames, string descriptors and interior array views, pending operations, retained outcomes, and observer queues. Frames survive native suspension and unlink on every normal or thrown completion; freed adjacent blocks coalesce for reuse. Canonical post-return still releases the complete serial invocation.
            - Capped tests first reproduced exhaustion, then passed 20,000 allocating text iterations and 2,000 stored-Promise iterations under 512 KiB. Additional tests retain operands across calls, return payloads through finally, interior split/slice views, and real suspended producers and observers through collection, rejection, and disposal. Raw allocator tests cover alignment, failed growth, invalid old ranges, shrinking, free/reuse, and 2,000 reallocations under 64 KiB.
            - Verified the full Cargo suite, Clippy, and local aarch64-darwin Nix flake checks. The remaining cutover audit covers supported native transfers below; escaped callbacks are deferred under D5.
          - [x] **R4.4b.ii:** Verified HTTP/text and native-transfer ownership alongside the existing filesystem, stdio, environment, and argument suites. HTTP response/header views survive collection; handler storage survives publication through consumer completion. Repeated resolved-world calls pass under 512 KiB, including recoverable failures; traps and interrupted calls require store disposal. Returned stream/future and callback ownership remain deferred.
        - Repeated compiler compilations show no retained heap growth above the test threshold; this is a regression measurement, not proof for every compiler path.
        - Verified allocator failure handling in `test_string_allocator_failure_does_not_advance_heap` preventing heap advancement on allocation failure.

**Retire:** UTF-16/WTF-8 runtime string storage, surrogate-half operations, lossy
conversions that hide invalid input, and compiler shortcuts that preserve the old
contract as their consumers migrate; remove obsolete legacy code rather than
converting it first. Paired-escape decoding remains an input codec, not an internal string mode.
Use contract-specific Unicode expectations while retaining matching Node comparisons.

### R5 — Typed Capability Lowering with a Small Rust Trait


- [x] **Lower resolved capability operations through a shared, consumer-shaped compiler contract.**
    - [x] **R5.1:** Derive the trait from two concrete consumers, such as clocks and the first filesystem operation. Group typed operations by responsibility and carry the operands, result/error behavior, and imports their lowering needs. Choose methods and context from those implementations; defer registration, dynamic loading, packages, and feature combinations until a consumer demonstrates a need.
      - `LowerCapability` describes typed clock and random operations with source parameters/results and canonical import adapters. Declaration validation, core signatures, and component wiring consume the same plan; call arity and core operand types are checked before emission.
      - `waitFor` preserves real P3 suspension. `randomNumber` converts the high 53 bits of `get-random-u64` to `[0, 1)` without allocation. These operations have no WIT domain errors; host failures trap independently of guest catch handlers.
      - Verified all 32 pipeline tests, including controlled random words, the independent Wasmtime P3 random bindings, signature diagnostics, and host failure propagation. `cargo clippy --lib --tests -- -D warnings` passes under `nix develop`.
    - [x] **R5.2:** Keep language values, exceptions, async suspension, stream transfers, and ABI ownership in shared lowering/support code. Capability implementations use these mechanisms and R4.2a's helper ABI/build/link foundation where guest algorithms are needed; they do not own separate schedulers, Promise engines, or stream registries. Verify a mixed-capability source task to expose misplaced responsibilities early.
      - A real P3 clock/random task retains and transforms large UTF-8 strings across suspension, returns `Result<string, number>`, and runs `finally` for both success and recoverable guest failure. Forty-eight calls reuse one instance; cancellation discards the store and never delivers stale cleanup into a new instance.
      - Capability plans contain no runtime state. Calls, exception completions, await continuations, canonical text allocation, and linked Unicode helpers remain shared. The serial-invocation/store-disposal lifecycle remains explicit; native stream consumers extend this boundary in R7.
      - Verified 66 pipeline/string tests and Clippy under the pinned Nix toolchain.
    - [x] **R5.3:** Verify bound aliases, shadowed built-ins, unrelated member names, dynamic forms that are exposed, and argument side effects. Inspect pure and mixed tasks' component imports and instantiate with only the requested interfaces. Retain ABI, indirect-call/global references, and initialization dependencies correctly; add finer pruning only when measured output needs it.
      - SWC binding identities distinguish `perry:clocks` / `perry:random` imports, named aliases, namespace literal members, builtin `Math.random`, and local shadows before Perry lowering. A typed side map connects collision-free generated bindings to capability plans; source names do not select capabilities after normalization.
      - Capability function values, dynamic member names, spread calls, default/rest parameters, and class initialization are explicitly diagnosed. Plain unrelated functions retain their behavior; unrelated member names never acquire capability imports.
      - HIR expression traversal keeps references from all emitted functions and prunes unused extern declarations. Canonical roots and linked helper data/global/table/initialization dependencies remain handled by the existing shared linker; helper and mixed-string regressions pass.
      - Added `types/p3.d.ts`, catalog support scoped to the WAFFLE Rust API, and source-contract documentation. Independently typed host callbacks verify duration conversion and shared adapters; an identical task under Node verifies argument/start/await order.
      - Verified the complete `cargo test` suite, `cargo clippy --lib --tests -- -D warnings`, and `nix flake check --no-update-lock-file` on aarch64-darwin. Other platforms were not executed locally.

**Retire:** String capability markers, generic `mem_call` capability routing,
and dispatcher-combination specialization for replaced operations. Conservative
reachability remains useful where support code needs it; a feature-matrix runtime
or replacement plugin architecture is not the goal.

### R6 — Production Async Tasks and Ownership

- [ ] **Support concurrently pending typed tasks through the production resolved-WIT path.**
    - [x] **R6.1:** Store async WIT imports and native capability operations, execute eagerly to first suspension, and retain one settlement for repeated observations. Native P3 threads share guest memory and typed root frames. Async application WIT imports and timers retain explicit canonical subtask handles, register once, and release their waitable sets after terminal acknowledgment. Canonical scratch remains rooted while siblings collect. Controlled host gates prove overlapping WIT imports and HTTP requests, including indirect async arguments and scalar/void results. Repeated filesystem, timer, HTTP, and typed import calls release native resources under a fixed memory cap (`tests/waffle_wit_native_test.rs`).
    - [ ] **R6.2:** Complete typed `Promise.all`, `Promise.allSettled`, and `Promise.race` over dense homogeneous arrays and statically typed tuples. Preserve input order, rejection, repeated observation, empty-input behavior, and eager execution. Use shared settlement records and completion notifications: register each operand once, with no sequential-await join, repeated pending-list reconstruction, or repeated result copying.
      - Implemented ordered results, fail-fast numeric rejection, settlement records, homogeneous arrays, heterogeneous tuples, and retained aggregate identity. Node/P3 comparisons cover these cases. Controlled Node reaction traces, empty-race ownership, and repeated-call storage checks pass. The scheduling measurement in PERFORMANCE.md covers 1–256 tasks; cancellation races remain open under R6.3.
      - `race` does not cancel losers. Every losing operation remains owned until completed or explicitly cancelled.
    - [ ] **R6.3:** Enforce ordinary-work lifetime at the owning-call boundary. Diagnose provable escapes and reject unresolved pending work at runtime. Provide cooperative cancellation for supported native APIs; retain buffers and owners until completion or cancellation is acknowledged. Verify completion/cancellation races, rejection, multiple observers, and bounded repeated-call storage.
      - [x] **R6.3a:** Retain settlement and operand roots across suspension and collection. Reject unresolved work before result delivery and prevent public calls from executing concurrently; direct blocking imports serialize through the host, and runnable overlapping entries fail the invocation guard. Traps and interrupted invocations require store disposal.
      - [ ] **R6.3b:** Exercise fan-out/join, event-versus-timer waits, retries, explicit loser cleanup, and replay-shaped imports against independently authored controlled hosts. A host may supply recorded outcomes; Perry retains no durable state or native handles across restarts. Duroxide supplies scenarios only, with no compiler-specific SDK, workflow protocol, or WIT package.

General callback/thenable compatibility remains D5. Detached work and overlapping
public invocations remain D6. Native cancellation follows the Component Model
concurrency contract; discarding a race result never authorizes implicit cancellation.

### R7 — Shared Native Streams through Real I/O

- [ ] **Process and forward data incrementally through P3/native stream boundaries with bounded storage.**
    - [x] **R7.1:** Extend the fixed-buffer experiment through an actual P3 I/O capability, starting with a file or HTTP workload. Exercise read and write/forwarding paths against resolved WIT and a real producer/consumer; choose batch sizes and adapter ownership from that workload. Check stream types and ownership against independent bindings, adding a composed-component probe where host fixtures leave a contract untested.
      - [x] **R7.1a:** Validate native filesystem stream forwarding against Wasmtime's independent P3 bindings. A component probe forwards exact binary files from zero bytes through 4 MiB with one 64 KiB guest memory page. Repeated successful copies, directory-read failures, and rejected directory writes consume both completion futures and release every descriptor and native task. A failed writer closes its input while the read future can report success; neither EOF nor a successful read completion alone establishes a successful copy.
      - [x] **R7.1b:** Compile native ByteStream source inputs with shared 8 KiB reads and checked byte access. An independently composed P3 filesystem producer supplies a compiled source scanner; both scan and native forwarding cover files up to 4 MiB with 64 KiB per guest memory. Entry ownership survives helper calls and finally blocks, then closes the readable end. Delayed/partial input, empty application chunks, early returns, numeric errors, host traps, store disposal, serial calls, queued overlapping calls, and allocating Unicode strings are tested. R7.2/R7.3 track required shared transfers and R8.2 covers filesystem source APIs. Complete source Web Streams and returned-stream ownership under R7.2/R7.3; general detached producers remain D6.
    - [ ] **R7.2:** Share byte buffers, views, transfer and completion handling across required capabilities using Web Streams, async iteration, and supported promise-based piping operations. Prove exact binary bytes, partial transfers, empty chunks, EOF, and separate failure/completion channels; EOF alone need not mean success. Text decoding must preserve scalars split across chunks and reject malformed or unfinished sequences. Returned P3 streams retain their transfer owners until completion or acknowledged cancellation. Preserve backpressure, partial transfers, stream locking, and separate EOF/completion failure. Buffered conveniences grow on demand within available guest memory, with checked allocation arithmetic; general detached producers remain D6.
      - [x] **R7.2a:** Add shared mutable byte views to source lowering. Numeric/literal/copy constructors, byte mutation, subarray aliases, independent slices, view identity, lengths, bounds, argument effects, and numeric wrapping match Node in component tests. Tracing retains backing allocations across helper returns, native reads, and thousands of temporary allocations under a 64 KiB memory cap. Invalid lengths unwind and permit instance reuse. Unsupported coercions, buffer overloads, and stored byte outcomes remain diagnosed.
        - [x] **R7.2a.i:** Carry Uint8Array parameters and results through canonical `list<u8>`, including numeric WIT errors and mixed scalar-text parameters. Tests preserve arbitrary binary data and returned subview ranges across repeated 64 KiB-capped calls. Both direct and stored-primitive-task entry adapters copy byte results before post-return reclamation; native task state is empty after completion.
      - [x] **R7.2b:** Finish connecting byte views to shared read/write transfers and capability completion channels for required consumers, reusing the completed incremental strict UTF-8 decoder and invocation-scoped ownership.
        - [x] **R7.2b.i:** Share native input reads with managed guest memory. Immediately awaited `readInto(input, destination)` fills the visible byte-view prefix, retains backing storage through suspension and collection, and returns a partial-transfer count without conflating empty destinations with EOF. Chunk reads reuse the same transfer implementation and a lazily allocated traced buffer. Tests cover subviews, unchanged tails, empty/partial/delayed input, mixed chunk/view reads, finally cleanup, returned byte views, numeric errors, repeated 64 KiB-capped calls, and real P3 files through 4 MiB. Stream and byte-view argument types are checked before lowering core handles.
        - [x] **R7.2b.ii:** Add strict incremental UTF-8 decoding through source `TextDecoder`. Source binding resolution preserves streaming options before Perry can discard them. Traced decoder state retains incomplete scalars and unread input after errors; immutable text results preserve scalar counts, BOM behavior, and input mutation independence. Differential tests cover valid and malformed sequences, boundaries, constructor labels, option effects, duplicate properties, and explicit Node differences. Native input larger than guest memory, delayed transfers, EOF validation, numeric errors, and repeated calls pass under a 64 KiB cap. Opaque bindings cannot change logical type across control flow.
        - [x] **R7.2b.iii:** Finish shared writes and source capability completion channels. Input transfer counts alone do not establish producer success; pending output owners remain live until their transfers and capability completion finish.
          - [x] **R7.2b.iii.a:** Share native writes through P3 stdout/stderr. Immediately awaited `perry:stdio` byte writes and single-string console output preserve view ranges, arbitrary bytes, UTF-8, NULs, and argument effects. Both the writable end and capability completion future close before the source call returns. Independent Wasmtime bindings verify partial writes, delayed readiness/flush, numeric errors, finally cleanup, reuse, disposal, and 4 MiB input-to-output forwarding under a 64 KiB guest cap. Controlled completion verifies that successful transfer can still end in a later capability error. Suspended output owners survive collection by sibling tasks; direct logging matches Node. Formatting, additional console argument forms, and stored byte-output Promises remain diagnosed.
          - [x] **R7.2b.iii.b:** Extend shared transfer/completion handling to R8.3's HTTP consumers, checking producer and consumer outcomes where applicable. Filesystem writes and reads already use it in R8.2a/b. Complete returned-stream ownership and acknowledged cancellation under R7.2/R7.3.
    - [ ] **R7.3:** Use delayed producers and slow/abandoned consumers to verify backpressure, cancellation by the documented disposal policy, early return, error cleanup, and ownership of both stream ends for supported transfers. Retain larger-than-guest-memory checks for incremental forwarding and repeated-call checks for bounded live storage. Buffered APIs grow on demand within available guest memory; they must not reserve untrusted advertised lengths upfront. HTTP GET and handler suites now cover the remaining transitions, including independent completion failure, abandonment, and store disposal. Public calls remain serial. Add explicit per-stream cancellation and returned-owner tests; general overlapping public calls remain D6.

**Retire:** P2 stream-resource polling, readiness subscriptions, and bespoke
`guest_stream_step`/body-forwarding protocols for migrated consumers. Keep any
source-level stream wrapper limited to its advertised semantics; Web Streams,
async iteration, and zero-copy claims need their own consumer evidence.

### R8 — Migrate the Required Static Capability Consumers

- [ ] **Make required capability tasks work through typed lowering and the static contract.**
    - [x] **R8.1:** Retain clocks, randomness, and direct waits; narrow Date to immutable UTC operations, support strict ISO timestamps and typed Temporal consumers, and finish read-only environment/arguments. Preserve time units, random-fill view identity/quotas, and bounded storage. Validate inputs before side effects and use controlled host input for nondeterministic behavior. Context mutation and callback timers are deferred under D1/D5.
      - [x] **R8.1a:** Migrate `performance.now`, `Date.now`, `crypto.getRandomValues`, and `crypto.randomUUID` alongside the existing wait and numeric-random operations. Preserve signed epoch and fractional monotonic milliseconds, visible byte-view identity and quotas, P3 short reads, strict native progress, scoped imports, and values retained through suspension and collection. Controlled host values/failures, Node-compatible outcomes, repeated bounded-memory calls, and disposal pass independent Wasmtime P3 bindings.
        - Verified all 390 Cargo tests, including ten new platform tests, Clippy, formatting, and all eight local aarch64-darwin Nix checks. SDK notes and the catalog record the supported byte-view subset, numeric validation errors, and native-failure contract. Date and context work is recorded below; callback timers are deferred under D5.
      - [x] **R8.1b:** Restrict Date to `Date.now()`, `new Date(epochMs)`, `.getTime()`, and `.toISOString()`. Prefer strict UTC ISO-8601 timestamps for interchange and the formal TC39 `Temporal.Instant` / `Temporal.PlainDateTime` APIs for typed date/time work. Preserve supported error channels, immutable values, and ownership through suspension; legacy Date behavior is deferred under D8.
        - [x] **R8.1b.i:** Enforced the four-operation Date subset with a statically numeric epoch-millisecond constructor. Omitted, nonnumeric, copy, and multi-argument constructors, calendar getters, `valueOf`, setters, and string parsing are diagnosed. Removed Date coercion and getter dispatch; ISO formatting computes its Gregorian fields directly. Clock, 516 epoch/ISO comparisons, invalid-date cleanup, suspension/disposal, and 64 KiB lifetime tests pass. README, SDK notes, and catalog describe the narrowed P3 contract.
        - [x] **R8.1b.ii:** Connected the allocation-free `datealgo`/`ixdtf` codec to typed [Temporal.Instant](https://tc39.es/proposal-temporal/docs/instant.html) string/epoch-millisecond factories, `epochMilliseconds`, and `toString()`, plus [Temporal.PlainDateTime](https://tc39.es/proposal-temporal/docs/plaindatetime.html) string parsing, ISO fields, whole-day addition, and `toString()`. Instants preserve nanoseconds; plain values have no time zone. Source fixtures enforce strict `YYYY-MM-DDTHH:mm:ss[.1–9 digits]Z` interchange and timezone-free whole-day arithmetic with parse/range errors. Typed records, retained Promises, controlled native suspension/disposal, and repeated allocation pass under 256 KiB. Invalid forms are diagnosed; parsing/range/annotation errors use catch/finally. These fixtures cross P3 string/number/result boundaries; broader application integration remains R9 work. Broader calendars, zones, options, and unused methods remain D8. README, SDK declarations, and catalog match the supported subset.
      - [x] **R8.1c:** Read-only environment, arguments, and initial working directory preserve cached snapshot identity, aliases, string reads, optional missing-key results, enumeration/JSON, and valid UTF-8 across repeated calls with bounded storage. All environment and argument mutation is deferred under D1.
        - [x] **R8.1c.i:** Migrate cached `process.argv` reads and `process.cwd()` through selected P3 environment imports. Retained heap frames keep instance-local snapshots alive beyond canonical post-return while invocation temporaries remain reclaimable. Argument aliases preserve identity through helpers, stored Promises, collection, suspension, and repeated exports. Empty lists, arbitrary valid UTF-8/NUL strings, absent versus empty directories, scoped imports/shadows, numeric failures, native traps, and store disposal pass independent host bindings. Initial directory defaults to `/` only when absent; arguments have no synthetic Node prefixes.
          - Verified cached snapshots across 200 calls under 64 KiB. A packed owned string backing keeps 4,000 arguments live through 20 exports and 60,000 allocating loop iterations under 2 MiB. Node comparisons account for scalar rather than UTF-16 lengths.
        - [x] **R8.1c.ii:** Source alias analysis rejects assignment, deletion, mutating methods, and bulk writes to `process.env`/`process.argv`, including casts, helper parameters/results, containers, optional access, and iteration. Missing environment keys preserve `string | undefined`. Independent copies remain mutable. Removed environment coercion and its JSON formatter dependency; environment snapshots survive collection, suspension, repeated calls, and disposal under 64 KiB. Verified 433 Cargo tests, strict Clippy and SDK declarations, formatting, and all eight local aarch64-darwin Nix checks.
          - [x] **R8.1c.ii.a:** Cache the P3 environment and retain its string values through aliases, exports, collection, and suspension. Independent host bindings cover lazy imports, empty environments, duplicate native keys, UTF-8/NUL values, native traps, retained Promises, and disposal. JSON snapshots are verified in R4.3b.ii.
      - **R8.1d — Deferred to D5:** One-shot and recurring callback timers and their capture ABI need a concrete acceptance consumer. Direct waits satisfy the initial timer surface; callback migration is not a cutover prerequisite.
    - [x] **R8.2:** Migrate filesystem consumers with preopen confinement, exact binary I/O, and valid UTF-8 text boundaries. Preserve options and argument effects, honoring or rejecting permissions/flags/encodings before I/O. Verify denial, invalid text/options, resource cleanup, repeated calls, and unrelated user functions through the public component path. Split metadata or other gaps according to actual consumers without closing the filesystem promise early.
      - [x] **R8.2a:** Migrate overwrite-only source `writeFileSync` over shared native P3 writes and a separate completion future. Named, namespace, and default `fs`/`node:fs` bindings preserve user shadows and prune unused imports. Exact UTF-8 and binary subviews, source-order argument/options effects, duplicate fields, encoding/flag rejection before I/O, normalized longest-prefix preopens, symlink confinement, read-only denial, and numeric errors pass public component tests. Partial transfers, delayed completion errors with allocating error payloads, disposal, sibling collection, and thousands of writes retain owners under a 64 KiB guest cap. Supported writes match Node. SDK and catalog scope this subset to the WAFFLE API.
      - [x] **R8.2b:** Migrate `readFileSync` binary and strict UTF-8 consumers, preserving BOMs, read options, producer completion failures, returned-value lifetimes, and repeated-call resource cleanup. Reads must not turn EOF into successful completion or malformed text into replacement characters.
        - [x] **R8.2b.i:** Read files as bytes or strict text when the encoding is omitted or literal. Shared native reads distinguish transferred bytes from closure and separate producer completion; the decoder and filesystem reuse one UTF-8 validator. Tests cover arbitrary binary data, exact BOM/NUL text, malformed sequences, partial/delayed input, later completion errors, read-only access, invalid options before I/O, stored text tasks, host traps, and disposal. Returned views and text survive collection, thousands of success/error calls fit a 64 KiB cap, and 4 MiB reads demonstrate materialization without a fixed file-size buffer. Valid text matches Node; strict malformed-text rejection is explicit.
        - [x] **R8.2b.ii:** Runtime string encoding labels return a tagged string-or-byte value. Kind-preserving locals, aliases, helper parameters/results, assignments, typeof guards, length, truthiness, strict equality, writes, and retained Promise outcomes pass public-component tests. Guards invalidate on assignments and merge conservatively across loops and exception paths; type-specific operations require narrowing. Component parameters/results export a text-or-bytes variant, including numeric-error Result returns. Delayed producers, separate completion failures, repeated awaits with collection, malformed UTF-8, runtime option effects, and repeated bounded-memory calls preserve values and release resources. Reusable option objects are completed in R8.2c.
      - [x] **R8.2c:** Migrate the promised metadata/directory operations and general option objects, preserving effects and rejection before I/O. The literal-option write subset in R8.2a does not close these legacy consumers or the filesystem parent.
        - [x] **R8.2c.i:** Migrate `statSync`, `existsSync`, `mkdirSync`, `unlinkSync`, `rmdirSync`, and `readdirSync` with supported literal options. A shared preopen resolver preserves normalization, longest-prefix selection, symlink confinement, read-only denial, and preopen-root protection. Stats preserve unsigned sizes, signed modification milliseconds, missing timestamps, type methods, and identity. Directory arrays grow with their contents, omit dot entries, and await the entry stream's separate completion. Stats, arrays, pending buffers, and names survive helper calls, suspension, and collection. Public-component tests cover option effects/duplicates, rejection before I/O, allocating native variants, delayed failure, traps/disposal, large directories, repeated bounded-memory operations, and matching Node behavior.
        - [x] **R8.2c.ii:** Reusable plain filesystem option objects preserve source-order and duplicate-field effects, aliases, typed helper parameters/results, mutation, optional undefined fields, and current-field validation before I/O. Tagged property storage traces nested values and reclaims dead cycles; objects and optional string properties survive retained Promises and collection. Runtime `delete`, accessors, methods, spreads, computed literal keys, and custom prototypes remain explicit source diagnostics. Stats and string arrays cross canonical component parameters/results, including numeric-error Results, and remain valid through repeated awaits and allocating finally blocks. Independent host record/list bindings verify all descriptor kinds and Unicode/empty values. Controlled directory producers prove options and retained arrays survive suspension, sibling collection, separate failure, traps, and disposal; supported option behavior matches Node.
          - Verified all 380 Cargo tests (54 filesystem tests), strict SDK declarations, Clippy, formatting, and all eight local aarch64-darwin Nix checks. HTTP and read-only context remain open; callbacks and detached tasks remain D5/D6; required stream ownership is tracked in R7.
    - [ ] **R8.3:** Restore standard `fetch`, `Request`, `Response`, and `Headers` over official P3 HTTP and R7's shared transfers. Support GET/POST, typed options and headers, byte/string/stream bodies, redirect modes, and cancellation. Resolve fetch at headers; preserve single body consumption, locking, HTTP statuses, replacement UTF-8 text decoding, JSON values, and independent completion failures. Buffer incrementally until guest-memory exhaustion, with checked arithmetic and no fixed body cap or upfront Content-Length allocation. Incoming handlers accept/return standard source values, including owned streams. Test the same application TypeScript directly under Node; retire proprietary HTTP facades after migrating their useful coverage.
      - `fetch(string, typed options)` supports methods, header records, and retained concurrent string/byte uploads. Request data is snapshotted before suspension; names, header ByteStrings, and prohibited methods/bodies validate before I/O. HEAD and null-body responses release ownership without requiring consumption. It resolves at headers and retains the body stream plus separate completion channels until `text()` or `bytes()` consumes them. Status, `ok`, normalized URL, single consumption, replacement UTF-8, concurrent retained outcomes, failure cleanup, and incremental growth run through typed WAFFLE builders and allocation-free Rust codecs. A controlled gate proves headers-first resolution using the same TypeScript under Node and P3; repeated calls run under a 512 KiB memory limit. The remaining R8.3 requirements above are open. Typed `perry:http` GET uses shared bounded transfers, explicit request coordinates, string-valued headers, and a caller-supplied body cap. Buffered responses preserve status, duplicate headers, and binary bytes; separate body/request completion settles before native resources are released. Source tests cover exact limits, overflow, repeated 4 MiB bodies under a 16 MiB guest cap, invalid metadata, truncated responses, strict decoding, argument order, collection, and retained body/header views. The independent `http_json.ts` fixture checks status, a single JSON Content-Type, a 64 KiB cap, UTF-8 and JSON validation. Native disposal probes cover pending headers/body. Stored HTTP requests overlap through native tasks; controlled-host tests verify separate completion and resource-free reuse. Source Web Streams and cooperative cancellation remain R6/R7 requirements. Resolved worlds now bind HTTP through the standard component encoder; independent tests verify typed results, binary failures, pending-header/body disposal, and resource-free reuse.
      - `compile_http_handler` exports the native P3 handler from typed request/response records with explicit byte caps. Shared transfers preserve duplicate headers and binary bodies; response publication precedes transmission, and invocation storage survives until separate consumer completion. Independent Wasmtime tests cover direct clock awaits, strict UTF-8/JSON, delayed producers, trailers, exact limits/overflow, metadata failures, consumer failure, abandonment, disposal, serial reuse, and bounded memory. Source exceptions trap; exposed streams, response trailers, and stored tasks remain outside this contract.

**Retire:** Each migrated capability's P2 bindings, name-based rewrites, and
capability-owned runtime/readiness adapter once its consumers are covered. Reuse
existing fixtures and behavior tests; do not require a fresh test file per variant
or expand into new APIs to declare migration complete.

### R9 — Production Cutover and Retirement

- [ ] **Make HIR → WAFFLE → P3 the production pipeline and remove its superseded dependencies and machinery.**
    - [ ] **R9.1:** Audit every behavior and test removed between `99760b7` and `a169308`, mapping it to executable current coverage, required restoration, or an already-approved deferral. Close the migration matrix with small, independently authored source/WIT tests for requirements learned from runner automation tasks, data-transform, calendar, and product-catalog. Runner remains an informative external reference and optional smoke test; never copy or commit its source or WIT definitions. Verify required static records, typed dictionaries, dense homogeneous arrays, finite unions, and validated JSON through P3. Enforce the subset with existing frontend checks and focused diagnostics for invalid fields/types, runtime `delete`, unconstrained `any`, and sparse/heterogeneous array operations, including aliases and casts. Check data-dependent bounds at runtime. Resolve actual consumer gaps before cutover; deferred compatibility does not block it, and unavailable fixtures remain unverified.
      - Runtime `delete` is rejected before source lowering, including aliases, casts, array elements, JSON values, and unused branches/functions; direct HIR compilation also rejects it. Removed the P3 object-deletion helper and deletion branches from value access. Dictionary enumeration uses insertion order for every key; integer-key classification/sorting and assignment snapshot buffers are removed. Ownership fixtures use optional fields or fresh records; matching Node comparisons remain alongside explicit numeric-key differences.
      - Resolved-world adapters and SDK signatures cover records, tuples, enums, variants, results, nullable options, typed lists, flags, exact `u64` transport, and directly awaited async calls. Invalid field/types, sparse/heterogeneous mutation, aliases/casts, unconstrained `any`, and data-dependent bounds have focused diagnostics or checked failures. Existing boundary suites verify retained values and resource-free reuse under 256–512 KiB.

      | Reference workload | Independent acceptance evidence |
      | --- | --- |
      | Calendar | `temporal_shift.ts`, `temporal_utc.ts`, and resolved WIT tests: strict ISO interchange and calendar arithmetic. |
      | Data transformation | `record_flow.ts`: bounded dense records, optional flags, selection, duplicate replacement, and Unicode labels. |
      | Product catalog | Record/optional-import fixtures and WIT list tests: typed records, optional values, domain variants, nested arrays, and host updates. |
      | Automation tasks | Record-flow host traces: awaited load → transform → save; failed loads/validation prevent writes; save errors propagate. Repeated calls retain bounded storage. |
      | HTTP/JSON interchange | `http_json.ts`, HTTP and JSON suites: byte limits, status/headers, strict UTF-8, value-tree validation, typed dictionaries, and completion/disposal errors. |

      Full runner components remain external optional smoke tests; no source or WIT is vendored.

    - [ ] **R9.2:** Use one P3 path across the CLI, public Rust API, Nix builds, and integration tests. Construct components from resolved, pinned official WASI WIT through the component encoder. Audit live references and ABI roots, then remove alternate framers, hand-maintained WASI replicas, obsolete fixtures, emitter patches, and production synthetic host hooks. Inventory WAT by responsibility: delete dead code, move pure algorithms to Rust helpers and control flow to shared typed WAFFLE builders, retaining only small ABI bridges with a named caller, ABI requirement, and validating test. Keep allocation on the existing guest heap. Application-declared WIT remains supported; built-in capabilities must not invent host protocols.
      - CLI, resolved-WIT APIs, HTTP handlers, and component fixtures use one `ComponentEncoder` implementation. Core-only compilation requires explicit WIT before component encoding. Removed textual component framers, the separate Promise runtime, and handwritten WASI HTTP/filesystem interfaces; synthetic fixture imports live in test infrastructure. P2 flake inputs, ambient resolution, linker registrations, and explicit host features are removed. The unused production mock-server binary is removed; HTTP acceptance uses test-owned ephemeral endpoints. Context caching, byte-view storage, input-stream ownership, stream transfer loops, and output completion use typed WAFFLE builders. Remaining runtime WAT still requires consolidation.
    - [ ] **R9.3:** Reconcile packaging, declarations, capability catalog, README, and architecture with executable behavior. Support named sync/async exports, local aliases/re-exports, multiple exports, and interface-qualified names with shared SDK/compiler naming and pre-encoding diagnostics. When the selected world exports `wasi:cli/run`, synthesize its adapter from top-level statements/await without requiring `runRun`. Static module initialization runs once per instance; retained bindings are shared by named exports and CLI entry, and asynchronous initialization is incompatible with synchronous WIT exports. Complete standard CLI exit behavior: numeric `process.exit`, instance-local `process.exitCode`, and failed command results with source cleanup are verified; finite validation errors and the remaining typed assignment shapes are still required. Make the capability catalog authoritative for supported shapes, WASI mappings, limitations, and executable Rust/Node test identifiers; completion checks must fail on missing evidence. Share WIT/world/entry configuration between component builds and SDK shells; reject ambiguous worlds. Generate disposable ignored `.perry` files before checking types without rewriting authored configuration. Include Node tools and test the same supported TypeScript under Node. Consumers pin Perry to an exact revision and inherit its dependency pins; separate consumer locks and generated declarations need not be committed. Verify fresh-checkout Nix builds and SDK generation leave tracked files unchanged. Keep useful performance history while removing obsolete migration claims from current documentation.
    - [x] **R9.4:** Published [reproducible cutover measurements](PERFORMANCE.md) and raw samples against the last legacy compiler (`eeb5655`). Independently authored text/I/O workloads verify output and resource cleanup over 255 serial calls each. Stripped components fall from 206,174/224,495 bytes to 9,927/25,319 bytes; P3 linear memory stays at 64/256 KiB, and median latency improves in both local workloads. The report records host/profile, cache, copying, and sampling limits. Measurement found a synchronous-WIT suspension trap: compilation now requires `async func` for exports reaching suspending capabilities, including through helpers, while preserving unrelated pure exports. All 361 regular tests, the opt-in comparison, strict Clippy, and local Nix checks pass. These historical measurements cover their recorded workloads; the broader regression audit remains open under R9.1.


**Retire:** The legacy compilation route and compatibility scaffolding that only
kept its implementation alive. Keep useful regression fixtures and any small
language/ABI support still justified by the established contracts.

## Complexity Deferred Until a Consumer Requires It

These are deliberately outside the cutover checklist. They are not failed tasks
to subdivide indefinitely, and are not marked implemented. Reopening one needs
a named consumer, a minimal source/WIT fixture, and a bounded acceptance condition
recorded in R9.1's matrix. Prefer an explicit typed operation over restoring general
JS compatibility. A callback or stream API can be statically typed and still be
deferred breadth; deferral does not mean it is inherently dynamic JavaScript.

| ID / Area | Cutover approach | Evidence needed to expand |
| --- | --- | --- |
| **D1 — Node context compatibility** | Read-only environment and arguments, optional missing-key reads, typed enumeration/JSON, and cwd. Defer all environment assignment/deletion, including string writes and guest-local mutations, plus cached argument mutation and coercion. | A production task needing mutation with an explicit ownership/host contract; legacy mutation or coercion tests alone do not qualify. |
| **D2 — Dynamic objects** | Declared record fields and fixed-value-type dictionaries; no runtime `delete`, prototypes, accessors, or general property reflection/order emulation. | A concrete typed data transformation that cannot reasonably use a record, optional field, or dictionary operation. |
| **D3 — Unconstrained `any` and coercions** | Known types and explicit finite unions; validate external values. Keep runtime tags only where useful for these contracts and JSON. | A concrete boundary or union missing from an accepted task, with explicit allowed types and failure behavior. |
| **D4 — Dynamic arrays** | Dense homogeneous arrays, checked allocation/bounds, and only consumer-required typed operations. No holes or heterogeneous mutation. | A concrete collection workload requiring another typed operation; arbitrary JS array edge cases do not qualify. |
| **D5 — Dynamic callback APIs** | Defer Promise constructors, arbitrary thenables, callback reactions, callback timers, and unconstrained callback captures. Typed combinators, concurrently pending native operations, and cooperative cancellation are required under R6. | A named production consumer with a reproducible fixture and bounded callback, ordering, and capture-lifetime criteria. |
| **D6 — Detached work and overlapping public calls** | Defer general detached tasks/producers and overlapping public invocations. R6/R7 require concurrent owned operations, explicit cancellation, and returned P3 streams retaining their transfer owners. | A named production consumer with a reproducible fixture and bounded ownership and reentrancy criteria. |
| **D7 — General JSON reflection** | R4.3b is complete: reuse its bounded value-tree codec and add only consumer-specific validation/narrowing. No schema-framework rewrite, arbitrary graph serialization, replacers, or revivers. | A real JSON interchange format that the existing codec and a small typed decoder cannot express. |
| **D8 — Additional API/network breadth** | Date is restricted to the immutable UTC subset: `Date.now()`, `new Date(epochMs)`, `.getTime()`, `.toISOString()`. Prefer strict UTC ISO timestamps and consumer-required `Temporal.Instant` / `Temporal.PlainDateTime` operations. Defer legacy Date constructors/coercions/getters, mutable setters, heuristic string parsing, host local-timezone adjustments, broader Temporal/calendar/time-zone APIs, and unused JS/Node APIs. Migrate required HTTP; TCP, UDP, TLS, and general events remain deferred. | A named workload requiring a specific operation or socket protocol, with explicit precision, parsing, calendar, and timezone semantics where applicable. |
| **D9 — Value/string storage sophistication** | Reuse typed support, existing tags, tracing, and checked adapters that pass supported ownership/memory probes. No blanket rewrite to eliminate boxing or every cycle-related mechanism. | A supported ownership graph or measured size/memory/performance problem the simpler representation cannot handle. |
| **D10 — Custom component tooling** | Existing WIT/binding/encoding tools plus isolated, tested gaps. | A working ABI consumer that those tools cannot express. |
| **D11 — Plugin systems, feature matrices, separate runtime packages** | One small capability-lowering trait and selected imports/support. | Demonstrated distribution constraints, not a growing list of capability names. |
| **D12 — Aggressive optimization and zero-copy designs** | Correct control flow, conservative dependency selection, and bounded transfers. | Measured component size, copying, memory, or throughput bottlenecks. |

Correct exceptions, Unicode semantics, ownership, cancellation behavior, and bounded
live memory remain acceptance obligations for every supported operation.
They are not deferred merely because a prototype works on the happy path.
Finish the roadmap when all required checkboxes pass for the static contract and
the legacy route is retired; this deferred register may remain populated.
