# Perry-WIT Architecture

Perry-WIT compiles static TypeScript into WASI 0.3 components through Perry HIR and typed SSA MIR built on [pliron](https://github.com/pliron-org/pliron).

## Pipeline

An authored or inferred contract supplies the resolved WIT world to five compiler stages:

```text
                  ┌───────────┬─── resolved WIT world ───┬────────────────────────┬────────────┐
                  ▼           ▼                          ▼                        ▼            ▼
TypeScript ──► Source ──► Frontend ──► Elaborate ──► Contract ──► Optimize ──► Codegen ──► Component ──► .wasm
                  1           2            3             4            5           6            7
```

| Stage | Representation | Role |
| :--- | :--- | :--- |
| 1. Source analysis | TypeScript AST | Binds `import { f } from "pkg:iface"` against the WIT world before import syntax is erased. |
| 2. Frontend | Specialized Perry HIR | Applies HIR compatibility rewrites, types WIT imports under their SDK declaration names, resolves types and specializes function instances. Preserves inferred binding origins for contextual completion. |
| 3. Elaboration | HIR to MIR | Completes types from context, fixes eager evaluation order and chooses MIR representations. Lowers `try`/`catch`/`finally` to shared unwinding blocks using a status-return convention rather than Wasm stack unwinding. Callers retain root ownership of pointer arguments while callees borrow parameters and return fresh allocations. |
| 4. Contract binding | pliron MIR and WIT | Binds WIT exports to MIR functions, checks their signatures and async requirements, and constrains numeric domains from the contract. |
| 5. Optimization | SSA MIR | Promotes local slots with `mem2reg`, specializes callees from `ProgramFacts`, and repeats canonicalization, SCCP, record SROA + `mem2reg`, DCE, and CFG simplification to fixed point (guarded by a lexicographical IR complexity measure). |
| 6. Code generation | Core Wasm (`pliron-wasm`) | Splits `async` functions into stackless ramp and step functions, structures control flow with Ramsey's dominator-tree method [[1]](#ref-1), folds single-use values into operand-stack trees, emits Canonical ABI marshalling, and links referenced runtime and standard-library functions. |
| 7. Component encoding | WASI 0.3 component | Emits Canonical ABI [[2]](#ref-2) lift and lower adapters, `cabi_realloc`, `cabi_post_<export>`, and for each async export the callback ABI entry points `[async-lift]<export>`, `[callback][async-lift]<export>`, and `finish_<export>`. |

Top-level statements collect into one function: the `wasi:cli/run` body in a command-only module, or a one-time initializer that `wasi:cli/run` and every export await before running. Module bindings persist across calls.

`ProgramFacts` infers numeric refinements (integer type and interval with two-step loop widening), purity and allocation effects, and parameter escape provenance across functions, including recursive call cycles. The numeric analysis propagates ranges forward and bitwidth demand backward [[3]](#ref-3).
Numeric refinements select `i32`, `i64`, or `f64` representations, specialize synchronous callees for the integer argument types known at each call site, and elide bounds checks on proven in-bounds array reads.
Closed non-escaping records decompose into scalar SSA values, read-only record parameters of internal synchronous functions promote to one scalar argument per field read, and `JSON.parse` / `JSON.stringify` on statically known shapes lower to specialized field decoders and schema-normalizing deep copies.
Repeated string appends reuse spare capacity in place when ownership is unique, and single characters read by indexing stay Unicode scalars through comparisons and appends.

During code generation, optional values avoid boxing by reserving sentinel encodings inside the value's own representation. `undefined`, `null` and an omitted property are the signaling-NaN payloads `0x7ff0000000000001` to `0x7ff0000000000003` in an `f64`, the addresses `0` to `2` for heap references, and `2` to `4` for booleans. WebAssembly arithmetic only produces quiet NaNs, so a computed number never matches a sentinel.
Constants rematerialize at use sites, single-use expressions in the same basic block stay on the WebAssembly operand stack when effect and trap ordering permits, and remaining values share WebAssembly locals across non-overlapping live ranges.

## Workspace

The compiler workspace separates these responsibilities:

| Crate | Responsibility |
| --- | --- |
| `perry-source` | Parses TypeScript, resolves local modules, and retains declaration locations. |
| `perry-contract` | Resolves WIT packages, derives contracts from source signatures, serializes WIT, and generates TypeScript declarations. |
| `perry-frontend` | Consumes shared source analysis, reads Perry HIR, applies compatibility rewrites, resolves types, and specializes instances. |
| `perry-lower` | Elaborates the frontend program into eager MIR, completes types from context and chooses representations. |
| `perry-mir` | Defines typed executable IR and its analyses. |
| `perry-codegen` | Lowers MIR to Wasm and implements Canonical ABI operations. |
| `perry-wit-diagnostics` | Holds source spans and portable diagnostics. |
| `pliron-control`, `pliron-wasm` | Provide control-flow and Wasm dialect infrastructure. |
| `perry-rt-build` | Builds the runtime workspace's artifacts for the compiler build script. |
| `perry-wit` | Adapts contract descriptions to compiler representations, binds exports, links runtime artifacts, and encodes components. |

The frontend returns a read-only program and metadata. Record identity and function instance specialization belong to this stage. Evaluation order, numeric refinement, effect analysis, allocation, ABI marshalling and linking belong to the compiler stages below it.

`perry-contract` depends on shared source analysis and portable diagnostics. Its dependency graph excludes frontend, lowering, MIR, code generation, and runtime build crates. Content functions operate on source buffers and return named artifacts; filesystem adapters acquire and write those contents.

## Memory

A component has one linear memory, and the compiler picks its heap strategy per module from the effect summaries of `ProgramFacts`.
`perry-wit --explain-tiers` prints the choice:

- **Task regions** when every reachable function allocates a bounded amount, cannot suspend or re-enter, uses no host resources, and the module has no top-level state. Each export call bump-allocates in chunks that grow from 4 KiB to 64 KiB, with a dedicated chunk per larger object, and frees them in bulk when the call ends. A synchronous export frees its region in `cabi_post_<export>`, after the host has lifted the results, so arguments and results go with the region.
- **Precise collection** otherwise. Objects up to 512 bytes allocate from non-moving 1 KiB pages across 11 size classes (powers of two and three-quarter midpoints) with mark bitmaps and lazy sweep. A larger object gets a dedicated page rounded up to a 1 KiB multiple, allocated from `talc` and marked and swept like the rest. Each record type carries a static pointer-offset descriptor so byte buffers and scalar payloads are never scanned for pointers. Compiled safe points clear dead reference slots in suspended async frames, publish exact live roots, and collect mid-call as well as when the last active task ends.

Non-escaping scalar records that still require an address (such as arguments passed to a non-retaining callee) pack into a single per-function activation frame (up to 448 bytes), reusing the same slots across loop iterations and freeing the frame on return.
For streams, backward liveness analysis detects when `reader.read()` or `writer.write()` results die before the next operation on the same stream, leasing and reusing a single promise, `{ done, value }` record, and chunk buffer in constant memory.

Strings are stored as validated UTF-8 byte sequences with cached Unicode scalar counts (precomputed in the data section for static literals) so character indexing avoids re-scanning ASCII strings.
Canonical ABI return areas are zero-initialized and rooted before the host populates them, and export adapters release canonical scratch storage only after `task.return` completes or all pending host operations acknowledge cancellation.

Static data starts at address `16`, above the reference sentinels, followed by relocated library data and the shared scratch stack. The `talc` allocator claims the remaining bytes of the initial 64 KiB page before growing memory, keeping small components within a single page.

## Async

Codegen splits each `async` function into a ramp function and a resume-state step function. Backward liveness analysis spills only values whose live ranges cross an `await` into the heap-allocated coroutine frame. Awaiting an already-settled promise continues inline without suspending to the host.

Each pending call has a task record holding its settlement state, outcome, and an intrusive observer list whose nodes live directly inside suspended coroutine frames or combinator records. A settled outcome is retained for repeated observation [[4]](#ref-4), §3, so later observers borrow it without copying its object graph.
Observers awaiting the same native operation share a single WASI 0.3 waitable handle, whose callback resumes them in registration order.
`Promise.all`, `allSettled`, and `race` register as observers of their inputs and count settlements, advancing arbitrarily deep dependency chains through host events with bounded guest stack depth.
Non-winning branches in `Promise.race` continue as background tasks, and the export adapter traps if unresolved background work remains when the top-level export finishes.

Readable stream readers decouple public read promises from native transfer completion: releasing a reader lock rejects pending reads while preserving buffered chunks for subsequent readers, explicit cancellation retains the native transfer owner and its pinned buffers until the host acknowledges cancellation, and `for await` loops cancel and unlock the stream on early exit while giving loop exceptions precedence over cancellation errors.

## Runtime

`runtime/` is a separate Cargo workspace containing the freestanding `#![no_std]` runtime and standard libraries. `perry-abi` supplies layouts, pure algorithms and the Wasm panic handler. `perry-rt-sys` supplies imports, guest-memory access and native stand-ins. `perry-runtime` owns allocation and instance state.

Standard-library functions implement UTF-8 validation, Unicode normalization, regex bytecode execution, JSON parsing, number and `Decimal` conversion, URL and HTTP header parsing, and calendar math. Each artifact package declares its builds in Cargo metadata. The host crate `perry-rt-build` reads those declarations, builds `wasm32-unknown-unknown` modules and generates the JSON WAT codecs. The compiler build script embeds runtime artifacts, and the independent contract crate embeds the pinned WASI WIT packages, so compiling TypeScript requires no Rust or LLVM toolchain. `PERRY_PREBUILT_RUNTIME_DIR` selects a directory of prebuilt runtime modules.

Runtime release builds optimize each artifact as one unit: optimization level 2, fat LTO, one codegen unit and aborting panics. The compact profile uses size optimization for the bounded runtime. Target-specific Cargo configuration applies position-independent code to Wasm dependencies; each artifact invocation supplies the shared-library linker arguments.

During linking, a reachability pass from the runtime and standard-library calls emitted by MIR strips unused functions, globals and element tables, compacts type signatures, and relocates the survivors into direct local `call` instructions that share the guest module's linear memory, scratch stack and `talc` allocator.

Runtime crates run their unit tests on the host in their own workspace. The compiler integration suite tests the embedded modules through the linker and Wasmtime.

## References

1. <a id="ref-1"></a>N. Ramsey. [*Beyond Relooper: Recursive Translation of Unstructured Control Flow to Structured Control Flow (Functional Pearl)*](https://doi.org/10.1145/3547621). Proceedings of the ACM on Programming Languages 6(ICFP), 2022.
2. <a id="ref-2"></a>WebAssembly Community Group. [*Canonical ABI Explainer*](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md). WebAssembly/component-model, living design document.
3. <a id="ref-3"></a>M. Stephenson, J. Babb, S. Amarasinghe. [*Bitwidth Analysis with Application to Silicon Compilation*](https://doi.org/10.1145/349299.349317). ACM PLDI, 2000.
4. <a id="ref-4"></a>B. Liskov, L. Shrira. [*Promises: Linguistic Support for Efficient Asynchronous Procedure Calls in Distributed Systems*](https://doi.org/10.1145/53990.54016). ACM PLDI, pp. 260-267, 1988.
