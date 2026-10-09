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
  - Checks reducibility.
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

At a component export without an error type, an escaping exception traps.

## Component boundary

`src/compiler/bind.rs` matches each WIT export with the exported TypeScript function of the SDK's implementation name; extra TypeScript exports stay internal. `perry_codegen::boundary` then emits, per export:

- the core function, which lifts flat arguments (or loads them from memory), calls the implementation and lowers the result (flat, or into a return area);
- `cabi_post_<export>`.

Supported WIT types are `bool`, integers, `f32`, `f64`, `string`, `list<T>` and records. Async exports and the remaining types follow the plan.

## Verification

- **Unit tests per crate:**
  - dialect round-trips and verifiers;
  - emitter tests executed in wasmtime;
  - runtime tests on the host.
- **End to end:**
  - `tests/mir_codegen_test.rs`: linked core modules with fuel metering;
  - `tests/component_test.rs`: WIT values through the component model.
- **Conformance:** `cargo run -p perry-conformance -- check`, against Node.js as the oracle, gated by the `passing.txt` ratchet.
