# Architecture

Perry-WIT compiles a static subset of TypeScript to WebAssembly components targeting WASI 0.3. This file describes the compiler as it is. The plan, design decisions D1–D6 and subset rules R1–R8 are in [TODOs.md](TODOs.md).

## Crates

| Crate | Role |
|---|---|
| `perry-wit` (root) | CLI, `compile_*` API, WIT binding (`src/compiler`), linker (`src/link`), component encoding (`src/component`), SDK generation (`src/sdk`) |
| `crates/perry-lower` | Perry HIR → perry MIR builder |
| `crates/perry-mir` | The `perry` dialect on pliron: types, ops, verifiers, the canonicalization pipeline |
| `crates/perry-codegen` | perry MIR → `wasm` dialect, value representations, canonical-ABI export glue (`boundary.rs`) |
| `crates/pliron-wasm` | The `wasm` dialect and the emitter to core Wasm; free of perry code so it can be published separately |
| `crates/perry-runtime` | `no_std` runtime linked into every module: allocator, task regions, strings, arrays |
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
| `T \| undefined` | as `T` | numbers: signalling-NaN payload (`perry_codegen::UNDEFINED_BITS`); references: `0`; booleans: `2` |

- **Records.** An `i32` shape id followed by 8-byte field slots in sorted field order.
- **Arrays.** `{ len, cap, data }`, with 8-byte element slots. Capacity is never 0, so a bounds-checked read is branch-free.
- **Strings.** `{ len (UTF-8 bytes), utf16, data, buffer }`.
  - `utf16` caches the ECMAScript length.
  - Constants are static objects in the data segment.
  - Concatenation appends in place to a shared growable buffer while the left operand is that buffer's newest string, so append loops are linear.

## Memory

All memory comes from linear memory managed by `perry-runtime`:

- **buffer.** talc 5.1 (`WasmDynamicTalc`): 1.9 KB of code, with in-place `realloc`. Chosen by measurement in `bench/allocators`.
- **task.** A region of 64 KiB chunks per export call, released in bulk.
  - The host's `cabi_realloc` for argument lowering opens the task.
  - The export adopts it.
  - `cabi_post_<export>` ends it after the host has lifted the result, so results can point into the region without copies.
- **instance.** The collector (TODOs.md P5.3) is not built. The builder cannot yet express module state (R4), so nothing outlives its task.

## Exceptions

There are no traps in the subset:

- `undefined` at a typed use throws a catchable `TypeError`.
- A write that would leave a hole throws a `RangeError`.
- A function that may throw returns an extra `i32` status after its results. Callers branch on it, propagating to the nearest handler or to their own caller.
- Inside a handler, `perry.caught_status` immediately follows the throwing operation. Its status feeds an explicit CFG branch, keeping handler liveness visible to SSA construction and the async split. Canonicalization preserves and verifies the pair.
- `perry.exception` holds Error/TypeError/RangeError objects or thrown numbers, booleans and strings. Error equality preserves identity; primitive equality preserves value semantics. A task-local pending reference carries the value alongside the status. Catch consumes it; rejected promises retain it for repeated awaits.
- `finally` runs for normal completion, return, throw, break and continue. A saved completion survives nested catches and suspension; an abrupt finalizer replaces it.

At a component export without an error type, an escaping exception traps. CLI commands return `err` instead.

## Component boundary

`src/compiler/bind.rs` matches each WIT export with the exported TypeScript function of the SDK's implementation name; extra TypeScript exports stay internal. `perry_codegen::boundary` then emits, per export:

- the core function, which lifts flat arguments (or loads them from memory), calls the implementation and lowers the result (flat, or into a return area);
- `cabi_post_<export>`.

Supported WIT types are `bool`, integers, `f32`, `f64`, `string`, `list<T>` and records; the remaining types follow the plan.

An `async func` export must be implemented by an async TypeScript function and uses the callback ABI (see Async).

For a world exporting `wasi:cli/run@0.3.0`, the builder creates an async entry function from the module's top-level statements. If an SDK command implementation is exported, the entry calls it after initialization and awaits its result when async. The adapter reads its boolean `ok` field; absent an explicit implementation, normal completion succeeds. Rejection or output failure returns `err`. Command locals belong to that invocation. Module initialization in other worlds is diagnosed until instance state is implemented. Typed `perry.intrinsic` operations cover console output, environment lookup/keys, arguments, clocks and process status. Only checked intrinsics carry a throwing effect. The helper linker forwards only reachable host function imports, shares identical imports with generated code and rejects signature conflicts.

`perry_runtime::process` stores an optional numeric exit code per instance. The setter validates safe integers before replacing the status and raises a catchable RangeError on invalid input. An explicit status takes effect after a successful command drains; rejection and an explicit error result retain precedence. `process.exit` calls a non-returning boundary helper that cancels native operations, frees the current task, and invokes `wasi:cli/exit.exit-with-code`. It never travels through exception handlers or finalizers. The WASI status is the numeric code modulo 256, defaulting to zero when unset.

`perry_runtime::output` keeps one ordered write queue per task and channel. A queued line owns its UTF-8 bytes through all partial writes and the host completion future. The callback dispatches stream-write and future-read events to the queue. Only async WIT exports can use console output; raw and synchronous exports diagnose it until they have a draining protocol. An output failure marks the task failed after its handles are closed.

## Async

Stackless state machines on the WASI P3 callback ABI. It uses no Component Model thread builtins and only the builtins of the default `component-model-async` feature.

- **Split.** Each async function becomes a ramp and a step function (`perry_codegen::async_split`).
  - The ramp allocates a frame in the task region, stores the arguments, creates the result promise and runs the step.
  - The step dispatches on the frame's state: 0 starts the body, `k` resumes after the `k`-th `await`.
  - Values live across an `await` live in frame slots.
  - `await` of a settled promise continues inline. Otherwise the frame waits on the promise and the step returns.
  - `return` fulfils the frame's promise; an exception rejects it.
- **Runtime.** A promise is `{ state, status, value, waiters }` (`perry_runtime::promise`). Settling queues its waiters on the task's run queue. `drive` runs queued frames, dispatching on the frame's kind.
- **Timers.** `setTimeout(delay, value)` from `node:timers/promises` is an async-lowered `wasi:clocks/monotonic-clock.wait-for`. A pending call joins the task's waitable set, and its promise settles when the subtask returns.
- **Exports.**
  - `[async-lift]<export>` enters the task, stores it with `context.set 0`, lifts the arguments, calls the ramp and finishes.
  - `finish` drives ready frames. It returns only after the result promise settles and all host subtasks and output queues drain, including work started without awaiting it. It checks unobserved rejections at task completion (per-turn rejection reporting remains a scheduler follow-up). Then it calls `task.return`, frees the task and returns `EXIT`. Otherwise it suspends the task and returns `WAIT` on its waitable set.
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
- **Conformance:** `cargo run -p perry-conformance -- check`, against Node.js as the oracle, gated by the `passing.txt` ratchet.
