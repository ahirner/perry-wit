# Architecture

Perry-WIT compiles a static subset of TypeScript to WebAssembly components targeting WASI 0.3. This file describes the compiler as it is. The plan, design decisions and accepted language profile are in [TODOs.md](TODOs.md).

## Crates

| Crate | Role |
|---|---|
| `perry-wit` (root) | CLI, `compile_*` API, WIT binding (`src/compiler`), linker (`src/link`), component encoding (`src/component`), SDK generation (`src/sdk`) |
| `crates/perry-lower` | Perry HIR → perry MIR builder |
| `crates/perry-mir` | The `perry` dialect on pliron: types, ops, verifiers, the canonicalization pipeline |
| `crates/perry-codegen` | perry MIR → `wasm` dialect, value representations, canonical-ABI export glue (`boundary.rs`) |
| `crates/pliron-wasm` | The `wasm` dialect and the emitter to core Wasm; free of perry code so it can be published separately |
| `crates/perry-runtime` | `no_std` runtime linked into every module: allocator, task regions, strings, arrays |
| `crates/json-codec` | Pure `no_std` JSON parser and serializer, shared by the runtime and standalone JSON helper |
| `crates/*-helper`, `src/helpers` | Computation helpers (JSON, time, number formatting, text, search, URL), linked on demand |
| `crates/conformance` | Contracts checked against Node.js as the oracle |

## Pipeline

```text
TypeScript ─ perry-parser ─ Perry HIR ─ perry-lower ─ perry MIR
   ─ canonicalize (mem2reg, DCE, simplify_cfg) ─ perry-codegen ─ wasm dialect
   ─ pliron-wasm emitter ─ core module (imports "__perry_helper")
   ─ src/link (relocate runtime + helpers) ─ wit-component ─ component
```

- **Builder.** Every TypeScript local becomes a `perry.local` slot, and pliron's `mem2reg` builds SSA from them, so the builder hand-rolls no SSA. Expression types come from operand MIR types. Records are identified by shape (sorted fields and their types). Unsupported constructs fail with a diagnostic; there is no fallback path.
- **Codegen.** A direct translation from `perry.*` to `wasm.*`, splitting blocks where exceptions and array growth need control flow.
- **Emitter.**
  - Makes control flow reducible (`pliron_wasm::reducible`): a cycle with several entries gets one guard block that dispatches on a label. Only resumes inside loops produce such cycles.
  - Structures the CFG with Ramsey's dominator-tree algorithm ("Beyond Relooper", ICFP 2022).
  - Folds single-use values into stack trees, keeping effects and traps in order.
  - Rematerializes constants.
  - Moves block arguments through the operand stack, so cyclic moves need no temporaries.
  - Groups locals by type.
- **Linker.** The runtime and helpers are compiled as position-independent wasm32 libraries by `build.rs`. `src/link` resolves `__perry_helper` imports against their exports, places their data after the module's static data, and reserves a shared stack.

## Values

| MIR type | Wasm | Notes |
|---|---|---|
| `f64` (number) | `f64` | ECMAScript arithmetic; `%` is `fmod` in the runtime |
| `i1` (boolean) | `i32` | |
| record, array, string | `i32` address | `0` is null |
| `perry.json` | `i32` address | Explicit null/boolean/number/string/array/object variant; optional JSON uses `0` for `undefined` |
| `T \| undefined` | as `T` | numbers: signalling-NaN payload (`perry_codegen::UNDEFINED_BITS`); references: `0`; booleans: `2` |

- **Records.** An eight-byte header holds a four-byte shape id and padding, followed by eight-byte field slots. Shapes use sorted field names; literal evaluation remains in source order. Access is a direct load/store, without descriptors, presence tables or JSON-view branches. Required fields may hold optional values; optional property presence and dynamic field addition/deletion are excluded.
- **Arrays.** `{ len, cap, data }`, with eight-byte homogeneous element slots and direct native access. Reads check dense bounds and return an optional value; writes append at length or reject holes.
- **Optional numbers.** Promoting a present number, reading a numeric collection, or converting a JSON number canonicalizes NaNs before they can be confused with the reserved absent-value payload. Finite values and signed zero are unchanged; an already-optional value retains its sentinel.
- **Bytes.** `perry.buffer` owns an ArrayBuffer descriptor `{ length, data }`; `perry.bytes` is a Uint8Array view `{ buffer, offset, length }`. Subarrays share storage; construction from bytes and slicing copy it. Range validation precedes set, and overlapping sets use memmove semantics. Descriptors and storage belong to the task, including across suspension. WIT `list<u8>` inputs become views over the canonical input storage; results point at the selected view span until task completion, without copying each element.
- **Text codecs.** `perry.text_encoder` and `perry.text_decoder` are distinct opaque task references. Encoding copies immutable UTF-8 bytes into independent mutable byte storage. Decoding copies the selected input view and pending bytes once, validates UTF-8 with Rust’s core validator, and adopts the complete prefix as an immutable string. The decoder retains the incomplete suffix, or unread queue after a fatal streaming error, together with continuation/BOM flags. A non-streaming call ends the sequence; the next call resets state. Only strict decoding is supported, including by default. The AST adapter resolves global codec names and substitutes reserved class names before the pinned HIR can discard receivers or options. Literal options keep source order and duplicate/unknown-property effects; unsupported option forms diagnose before lowering.
- **JSON.** Six explicit variants use task-owned trees, without dynamic NaN boxing. Parsed objects hold a fixed array of fields; lookups and existing-field mutation retain JSON aliases. Native-to-JSON conversion builds a snapshot using the known schema; decoding builds ordinary native records/arrays. Scalar conversion checks the variant and returns an unboxed value. Cross-representation alias identity, dictionaries, dynamic property insertion/deletion and recursive native conversion are excluded. Conversion is emitted only at the boundary; ordinary native access needs no JSON checks. Native serialization follows schema field order.
- **Compact JSON only.** Revivers, replacers and formatting are compile-time exclusions. The shared codec uses 32-byte nodes and bounded iterative serialization. Ancestor identities detect cycles in JSON trees; the runtime nesting bound is 128. Native schema expansion has a compile-time bound of 32. No callback traversal state, source contexts, ordered hash buckets or alias-validation descriptors remain.
- **Strings.** `{ len (UTF-8 bytes), scalars, data, buffer }`.
  - `scalars` caches the Unicode scalar count, making `.length` O(1). This is not a UTF-16 cache; `scalars == len` also identifies ASCII strings.
  - Constants are static objects in the data segment.
  - Concatenation appends in place to a shared growable buffer while the left operand is that buffer's newest string, so repeated tail appends take amortized O(total appended bytes + append count) work. Retained prefixes remain immutable; appending to an older prefix can copy. A rope is a possible later benchmark-driven optimization for branching concatenations, not a prerequisite for this representation.
  - Typed string intrinsics index Unicode scalars. ASCII positions are direct byte offsets; other positions scan UTF-8 scalars. `codePointAt` returns full scalar values. `charCodeAt`/`fromCharCode` are compile-time exclusions; no operation creates surrogate halves. Slice, substring, trim and split return task-owned headers over immutable source spans, retaining append-buffer ownership. Appending to a tail span preserves earlier lengths and contents.
  - Search returns scalar positions with distinct undefined/NaN defaults, and split applies the unsigned 32-bit limit conversion. Casing reuses the pure Unicode mapper and property tables already shared by the helper, including expansions and final sigma. Repeat/case output sizes are checked before allocation. All method arguments are evaluated once, in order, before argument conversions; ignored extra arguments retain their effects.
  - AST declaration spans identify unannotated HIR locals, so MIR values determine their types without discarding HIR hints used while lowering other expressions. This preserves optional builtin results in inferred record fields and dependent native collections. Explicit field, parameter and return types remain checked boundaries.

## Memory

All memory comes from linear memory managed by `perry-runtime`:

- **buffer.** talc 5.1 (`WasmDynamicTalc`): 1.9 KB of code, with in-place `realloc`. Chosen by measurement in `bench/allocators`.
- **task.** A region of 64 KiB chunks per export call, released in bulk.
  - The host's `cabi_realloc` for argument lowering opens the task.
  - The export adopts it.
  - `cabi_post_<export>` ends it after the host has lifted the result, so results can point into the region without copies.
- **instance.** The collector (TODOs.md P5) is not built. The builder cannot yet express module state, so nothing outlives its task.

## Exceptions

There are no traps in the subset:

- `undefined` at a typed use throws a catchable `TypeError`.
- A write that would leave a hole throws a `RangeError`.
- A function that may throw returns an extra `i32` status after its results. Callers branch on it, propagating to the nearest handler or to their own caller.
- Inside a handler, `perry.caught_status` immediately follows the throwing operation. Its status feeds an explicit CFG branch, keeping handler liveness visible to SSA construction and the async split. Canonicalization preserves and verifies the pair.
- `perry.exception` holds Error/TypeError/RangeError/SyntaxError objects or thrown numbers, booleans and strings. Error equality preserves identity; primitive equality preserves value semantics. A task-local pending reference carries the value alongside the status. Catch consumes it; rejected promises retain it for repeated awaits. Checked JSON intrinsics read that status immediately after the runtime call, keeping their exception edges explicit in MIR.
- `finally` runs for normal completion, return, throw, break and continue. A saved completion survives nested catches and suspension; an abrupt finalizer replaces it.

At a component export without an error type, an escaping exception traps. CLI commands return `err` instead.

## Component boundary

`src/compiler/bind.rs` matches each WIT export with the exported TypeScript function of the SDK's implementation name; extra TypeScript exports stay internal. `perry_codegen::boundary` then emits, per export:

- the core function, which lifts flat arguments (or loads them from memory), calls the implementation and lowers the result (flat, or into a return area);
- `cabi_post_<export>`.

Supported WIT types are `bool`, integers, `f32`, `f64`, `string`, `list<T>` and records; the remaining types follow the plan.

An `async func` export uses the callback ABI (see Async) and accepts synchronous or promise-returning TypeScript implementations.

A synchronous export drains ready promise continuations before lifting its result. An unhandled rejection or native work still pending at that boundary fails the call. Cleanup runs before region release. Blocking cancellation is forbidden while a synchronous export remains on the stack; if Wasmtime traps there, guest buffers stay live until the poisoned store is disposed. Work that needs a host suspension belongs in an async WIT export.

For a world exporting `wasi:cli/run@0.3.0`, the builder creates an async entry function from the module's top-level statements. If an SDK command implementation is exported, the entry calls it after initialization and awaits its result when async. The adapter reads its boolean `ok` field; absent an explicit implementation, normal completion succeeds. Rejection or output failure returns `err`. Command locals belong to that invocation. Module initialization in other worlds is diagnosed until instance state is implemented. Typed `perry.intrinsic` operations cover console output, environment lookup/keys, arguments, clocks and process status. Only checked intrinsics carry a throwing effect. The helper linker forwards only reachable host function imports, shares identical imports with generated code and rejects signature conflicts.

`perry_runtime::process` stores an optional numeric exit code per instance. The setter validates safe integers before replacing the status and raises a catchable RangeError on invalid input. An explicit status takes effect after a successful command drains; rejection and an explicit error result retain precedence. `process.exit` calls a non-returning boundary helper that cancels native operations, frees the current task, and invokes `wasi:cli/exit.exit-with-code`. It never travels through exception handlers or finalizers. The WASI status is the numeric code modulo 256, defaulting to zero when unset.

`perry_runtime::output` keeps one ordered write queue per task and channel. A queued line owns its UTF-8 bytes through all partial writes and the host completion future. The callback dispatches stream-write and future-read events to the queue. Only async WIT exports can use console output; raw and synchronous exports diagnose it until they have a draining protocol. An output failure marks the task failed after its handles are closed.

## Async

Stackless state machines on the WASI P3 callback ABI. It uses no Component Model thread builtins and only the builtins of the default `component-model-async` feature.

- **Split.** Each async function becomes a ramp and a step function (`perry_codegen::async_split`).
  - The ramp allocates a frame in the task region, stores the arguments, creates the result promise and runs the step.
  - The step dispatches on the frame's state: 0 starts the body, `k` resumes after the `k`-th `await`.
  - Values live across an `await` live in frame slots.
  - An uncontended settled `await` branches directly to its continuation, with no queue allocation or return from the step. Pending waits register the frame; settled waits yield to competing ready frames. The queue preserves pending waiter order, but synthetic ECMAScript microtask FIFO ordering is explicitly excluded.
  - `return` fulfils the frame's promise; an exception rejects it.
- **Runtime.** A promise is `{ state, status, value, waiters }` (`perry_runtime::promise`). Settling queues its waiters on the task's run queue. `drive` runs queued frames, dispatching on the frame's kind.
- **Timers.** `setTimeout(delay, value)` from `node:timers/promises` is an async-lowered `wasi:clocks/monotonic-clock.wait-for`. A pending call joins the task's waitable set, and its promise settles when the subtask returns.
- **Exports.**
  - `[async-lift]<export>` enters the task, stores it with `context.set 0`, lifts the arguments, calls the ramp and finishes.
  - `finish` drains ready frames, then checks unobserved rejections and output failures before another host turn. A failure cancels native operations and frees the task, returning a command error or trapping an export without an error result. A handler installed later in the same continuation queue can recover; one deferred until a later timer cannot. Checked rejection entries are discarded, keeping total scanning work linear.
  - Successful completion waits for the result promise and all host subtasks and output queues, including work started without awaiting it. Then it calls `task.return`, frees the task and returns `EXIT`. Otherwise it suspends the task and returns `WAIT` on its waitable set.
  - `[callback][async-lift]<export>` resumes the task from `context.get 0`, settles a returned subtask or advances a pending output transfer, releases completed handles and finishes again.
  - Calls may overlap: every task has its own region, run queue and subtasks.
  - A cancellation event detaches pending waitables, synchronously cancels timers and active output writes/reads, drops their handles, frees the task region, and acknowledges `task.cancel`. Output cancellation covers both stream transfer and the host completion future; queued lines never start. Sync cancellation may run other tasks, so runtime cancellation restores the original task after each blocking host call. Guest continuations and finalizers are abandoned.

## Verification

- **Unit tests per crate:**
  - dialect round-trips and verifiers;
  - emitter tests executed in wasmtime;
  - runtime tests on the host.
- **End to end:**
  - `tests/mir_codegen_test.rs`: linked core modules with fuel metering;
  - `tests/component_test.rs`: WIT values through the component model;
  - `tests/async_component_test.rs`: async exports with WASI P3 timers, including overlapping calls.
- **Conformance:** `cargo run -p perry-conformance -- check`, against Node.js as the oracle, gated by the `passing.txt` ratchet within the accepted profile. Four historical contracts are `exempt` with recorded reasons; unsupported APIs have rejection contracts. Node differential strings stay within BMP, while component tests assert Unicode scalar behavior for astral strings.
