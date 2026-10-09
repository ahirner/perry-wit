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
| `crates/perry-std` | Built-ins on top of the runtime (string methods, `Uint8Array`, JSON, random), linked on demand |
| `crates/perry-fs` | `node:fs/promises` over `wasi:filesystem`, linked on demand |
| `crates/perry-web` | `Headers`, constructed `Response`/`Request`, Web writers over stdout/stderr, linked on demand |
| `crates/perry-abi` | Runtime services and layouts the on-demand libraries share (no exports of its own) |
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
| `Uint8Array` (`perry.bytes`) | `i32` address | a view `{ len, data }`; `subarray` shares the bytes |
| JSON value (`perry.json`) | `i32` address | a node of the JSON helper's graph; `0` is `undefined` |
| `Date` (`perry.date`) | `f64` | the time value, held by value |

- **Records.** An `i32` shape id followed by 8-byte field slots in sorted field order.
- **Arrays.** `{ len, cap, data }`, with 8-byte element slots. Capacity is never 0, so a bounds-checked read is branch-free.
- **Built-ins.** The builder maps the standard library to `perry.intrinsic` services with fixed signatures (`perry_mir::intrinsics`). Codegen lowers them to Wasm instructions (`Math`, `Date` TimeClip), `perry-std` functions (strings with UTF-16 indices, `Uint8Array`, JSON access, random values) or helper libraries (Unicode case mapping, JSON parsing, number and date formatting). Services that may throw leave their status in the task for compiled code to check (`perry_status`). `perry-std` is a library of its own because the linker keeps reachable functions but a library's data segment whole: programs that use none of the built-ins carry none of their messages and tables. It reaches the runtime through `__perry_helper` imports, which the linker resolves between libraries.
- **JSON.** `JSON.parse` produces `perry.json` nodes; property and index reads stay JSON values, and a typed use converts them (R6). `JSON.stringify` of typed values is generated per type: one serializer function per record or array type writes structure and pre-formatted pieces to a runtime writer.
- **Promise combinators.** `Promise.all` and `Promise.race` are runtime frames that wait on every pending input at once; `drive` steps them like async frames.
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
- A function that may throw returns an extra `i32` status after its results (1 `TypeError`, 2 `RangeError`, 3 `Error`, 4 another thrown value, 5 `process.exit`). Callers branch on it, propagating to the nearest handler or to their own caller.
- Inside a `try`, the builder follows each throwing op with `perry.caught` and a branch to the handler, so the MIR CFG has the handler edges. Codegen routes that op's exception to a landing block instead of out of the function.
- `throw value` (`perry.raise`) makes an exception object pending in the task; `catch (e)` (`perry.catch`) takes it, or materializes an `Error` for failures raised without one. A rejected promise keeps the object for its awaiters. Catch bindings support `instanceof`, `.message`, identity and rethrow.
- `finally` bodies are emitted once per way out: normal end, exception (then rethrow), and every `return`, `break` and `continue` that leaves them.
- Status 5 (`process.exit`) passes through every handler.

At a component export without an error type, an escaping exception traps; a command reports it on stderr and fails.

## Component boundary

`src/compiler/bind.rs` matches each WIT export with the exported TypeScript function of the SDK's implementation name; extra TypeScript exports stay internal. `perry_codegen::boundary` then emits, per export:

- the core function, which lifts flat arguments (or loads them from memory), calls the implementation and lowers the result (flat, or into a return area);
- `cabi_post_<export>`.

Supported WIT types are `bool`, integers, `f32`, `f64`, `string`, `list<T>` and records; the remaining types follow the plan.

An `async func` export must be implemented by an async TypeScript function and uses the callback ABI (see Async).

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
  - `finish` drives ready frames and starts writing buffered console output. Once the result promise is settled and the outputs are closed, it calls `task.return` with the lowered value, frees the task and returns `EXIT`. Otherwise it suspends the task and returns `WAIT` on its waitable set.
  - `[callback][async-lift]<export>` resumes the task from `context.get 0`, then lets the runtime handle the event (a returned subtask, a completed stream write or the output's completion future) and finishes again.
  - Calls may overlap: every task has its own region, run queue, subtasks and output.
- **Commands.** Top-level statements become the async function `perry_main`, bound to `wasi:cli/run.run`. Its result is `err` after an uncaught exception (reported on stderr) or with a non-zero `process.exitCode`.
- **Host services.** Console output, `process.env`, the clocks and the async built-ins are called from `perry-runtime` (`host.rs`, `io.rs`); `src/link` forwards the imports of the runtime functions a program reaches. Codegen checks that the world imports each interface a service needs (`perry.intrinsic`).
- **Console output.** Per-task buffers drain through one `wasi:cli/stdout` or `stderr` stream each, with async `stream.write`. Output produced while a write is blocked goes to a second buffer. Closing drops the writable end and waits for the host's completion future, so all output is written before the export returns.

## Verification

- **Unit tests per crate:**
  - dialect round-trips and verifiers;
  - emitter tests executed in wasmtime;
  - runtime tests on the host.
- **End to end:**
  - `tests/mir_codegen_test.rs`: linked core modules with fuel metering;
  - `tests/component_test.rs`: WIT values through the component model;
  - `tests/command_test.rs`: commands with console output, environment, exit codes and exceptions;
  - `tests/builtins_test.rs`: built-in services against values recorded from Node.js;
  - `tests/async_component_test.rs`: async exports with WASI P3 timers, including overlapping calls.
- **Conformance:** `cargo run -p perry-conformance -- check`, against Node.js as the oracle, gated by the `passing.txt` ratchet.
