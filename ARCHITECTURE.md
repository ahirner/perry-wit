# Perry-WIT Architecture

Perry-WIT compiles static TypeScript into WASI 0.3 components through Perry HIR and typed SSA MIR built on [pliron](https://github.com/pliron-org/pliron).
Observable language and numeric behavior is specified in [Semantics](https://ahirner.github.io/perry-wit/docs/semantics.html) and [Numbers](https://ahirner.github.io/perry-wit/docs/numbers.html).

## Pipeline

The resolved WIT world feeds four compiler stages:

```text
                  ┌───────────────────┬───── resolved WIT world ───────┬────────────┐
                  ▼                   ▼                                ▼            ▼
TypeScript ──► Source ──► HIR ──► Contract ──► MIR ──► Optimize ──► Codegen ──► Component ──► .wasm
                  1        2          3         4          5           6            7
```

| Stage | Representation | Role |
| :--- | :--- | :--- |
| 1. Source analysis | TypeScript AST | Binds `import { f } from "pkg:iface"` against the WIT world before import syntax is erased. For a command world, wraps top-level statements in the `wasi:cli/run` export. |
| 2. HIR lowering | Perry HIR | Moves top-level statements into an initializer that runs once, before the first export call. Module bindings persist across calls. |
| 3. Contract resolution | HIR and WIT | Checks export signatures, requires `async func` for every function that can suspend, maps each native operation to the WASI import it uses, and lays out Canonical ABI scratch areas. |
| 4. MIR lowering | pliron MIR | `catch` and `finally` share unwinding blocks. Heap pointers live in root frames that survive suspension. |
| 5. Optimization | SSA MIR | `mem2reg` runs once, then canonicalization, SCCP, `ProgramFacts`, CFG simplification and DCE repeat until fixed point. |
| 6. Code generation | core Wasm (`pliron-wasm`) | Structures control flow with Ramsey's dominator-tree method ([Ramsey 2022]). Single-use values defined in the same block become expression trees on the operand stack. Links the referenced helper functions. |
| 7. Component encoding | WASI 0.3 component | Emits [Canonical ABI] lift and lower adapters, `cabi_realloc`, `cabi_post_<export>`, and for each async export the callback ABI entry points `[async-lift]<export>`, `[callback][async-lift]<export>` and `finish_<export>`. |

`ProgramFacts` infers numeric refinements (integer type and interval), effects and parameter provenance across functions, recursive calls included.
Numeric refinements select `i32`, `i64` or `f64` representations (see [Numbers](https://ahirner.github.io/perry-wit/docs/numbers.html)) and elide bounds checks on proven in-bounds array reads.
Threshold widening and dataflow presence tracking are planned.
`JSON.parse` and `JSON.stringify` on statically known shapes lower to specialized decoders and deep copies.

## Memory

A component has one linear memory, and the compiler picks its allocation strategy per module from the effect summaries of `ProgramFacts`.
`perry-wit --explain-tiers` prints the choice:

- **Task regions** when every reachable function allocates a bounded amount, cannot suspend or re-enter, and the module has no top-level state. Each export call bump-allocates in its own region, and the region is freed in bulk when the call ends. A synchronous export ends its call in `cabi_post_<export>`, after the host has lifted the results, so arguments and results go with the region.
- **Precise collection** otherwise. Allocations live in non-moving storage (size-class pages with mark bitmaps), compiled safe points publish exact roots, and pending host operations pin the buffers they use. When a call ends with no roots, pins or tasks left, the heap is collected.

Escape analysis assigns non-escaping records in synchronous functions to stack frame slots freed on return.
Canonical ABI buffers stay pinned while the host lowers a value; lifting moves the payload to an ordinary owner or frees it after a copy.

The linker places helper data and the shared stack at the start of linear memory. The `talc` allocator claims the remaining bytes of the initial pages before growing memory, and task regions allocate in 4 KiB chunks, keeping small components within a single 64 KiB page.

## Async

Each `async` call has a task record holding its settlement state, outcome and observers in the guest heap. A settled outcome is kept for repeated observation ([Liskov & Shrira 1988], §3); later observers borrow it instead of copying its object graph.
Observers of one promise share a single native completion, whose callback resumes them in registration order.
`Promise.all`, `allSettled` and `race` register as observers of their inputs and count settlements, so long dependency chains advance through host events with bounded guest stack depth.
A pending host operation keeps its buffers pinned until the host acknowledges completion or cancellation.

## Helpers

Freestanding Rust runtime and standard library helpers are precompiled to `wasm32-unknown-unknown` and embedded in the compiler binary.
Codegen copies only the functions and data sections a program references into the output module and renumbers their calls, sharing the guest module's linear memory, stack and allocator.

## References

- **[Liskov & Shrira 1988]**: B. Liskov, L. Shrira. *Promises: Linguistic Support for Efficient Asynchronous Procedure Calls in Distributed Systems*. ACM PLDI, pp. 260-267, 1988.
- **[Ramsey 2022]**: N. Ramsey. *Beyond Relooper: Recursive Translation of Unstructured Control Flow to Structured Control Flow (Functional Pearl)*. Proceedings of the ACM on Programming Languages 6(ICFP), 2022.
- **[Canonical ABI]**: WebAssembly Community Group. *Canonical ABI Explainer*. WebAssembly/component-model, living design document.

[Liskov & Shrira 1988]: https://doi.org/10.1145/53990.54016
[Ramsey 2022]: https://doi.org/10.1145/3547621
[Canonical ABI]: https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md
