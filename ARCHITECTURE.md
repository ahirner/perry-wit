# Perry-WIT Architecture

Perry-WIT compiles static TypeScript directly to WebAssembly components targeting WASI 0.3 (Preview 3) through Perry HIR and pliron typed MIR.

## Principles

1. **Ahead-of-Time Execution**
   TypeScript operations compile directly into native WebAssembly instructions. Primitive local variables map to SSA values and block parameters. Compound aggregates and retained objects live in managed guest memory.

2. **Static Source Contracts**
   Binding-aware validation rejects unsupported dynamic JavaScript patterns while the AST retains full contextual information for precise diagnostics. Lightweight runtime checks handle data-dependent array bounds and finite variant tags.

3. **Direct Intermediate Representation Lowering**
   Control flow constructs (branches, loops, exception paths, function calls, and suspension points) share a unified lowering and validation pipeline. Typed capability plans link source validation directly to canonical imports and runtime signatures.

4. **Native Concurrency**
   Suspension, resumption, and asynchronous transfers are managed by the host via WASI 0.3 Component Model primitives. Guest task records and observer lists maintain Promise identity, repeated observation, and combinator settlements. Continuations are scheduled directly by native P3 wakeups.

5. **Authoritative WIT**
   Pinned official WASI definitions specify built-in platform capabilities, while application WIT files define internal interfaces. The SDK and compiler share unified export conventions, ensuring helper names do not leak into host protocols.

6. **Unified Guest Heap**
   Pure algorithms rely on compact Rust helpers compiled directly into the guest module. Dynamic allocations, root frames, canonical scratch buffers, and suspended task states follow an explicit, single-heap ownership model managed by `talc`.

7. **Reproducible Evidence**
   Nix flakes pin all toolchain versions, compilers, and interface definitions. Differential testing against Node.js establishes source-level equivalence. Controlled WASI 0.3 hosts verify canonical ABI compliance, stackless coroutine suspension, resource lifetimes, and cleanup.

## Lowering

The compiler layers cooperate to transform high-level TypeScript into validated WASI 0.3 WebAssembly components:

```text
┌─────────────────────────────────────────────────────────────┐
│                     Compiler Frontend                       │
│    ESM Loader  •  Source AST  •  Static Source Validation   │
│    WIT Import Binding (bind_source)  •  Command Adapter     │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                     Perry HIR Lowering                      │
│    Lexical Scopes  •  Guarded Initializer  •  Control Flow  │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                     Contract Resolution                     │
│    Signature Checks  •  WIT World Match  •  Suspension Check│
│    Canonical ABI Plans  •  Scratch Buffer Layouts           │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                      pliron MIR Engine                      │
│   Block Joins  •  Exception Paths  •  Root Frame Tracking   │
│                                                             │
│   ┌─────────────────────────┐     ┌───────────────────────┐ │
│   │    Runtime Builders     │     │  Linked Guest Helpers │ │
│   │     Task Schedulers     │     │ Pure Rust Wasm (JSON, │ │
│   │     Stream Readers      │◄────┤ Time, Text, Search)   │ │
│   │        Host I/O         │     │ (Relocated Bytecode)  │ │
│   └─────────────────────────┘     └───────────────────────┘ │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                       Code Generation                       │
│    Dominator Tree Structuring  •  Reducible Control Flow    │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                  Component Model Encoding                   │
│    Canonical ABI Adapters  •  Resolved WIT  •  Encoder      │
└─────────────────────────────────────────────────────────────┘
```

### 1. Source Analysis

The frontend resolves local static ESM dependencies and validates TypeScript AST constructs. Lexical binding analysis distinguishes global built-ins from shadowed local identifiers. Static validation enforces read-only execution constraints and preserves operand evaluation order.

Before lowering to Perry HIR, the compiler interacts with the resolved WIT world:
- **Binding WIT Imports**: Resolves Component Model import specifiers (`import { ... } from "pkg:interface"`) against the WIT world and rewrites AST bindings before standard import syntax is erased.
- **Command Adapter Synthesis**: When targeting a world exporting `wasi:cli/run@0.3.0` and no explicit runner function exists, the compiler injects a command runner export wrapping top-level statements.
- **AST Contract Checks**: Validates that source export declarations match the shapes required by the target world.

### 2. Perry HIR Lowering

The normalized AST lowers into Perry HIR:
- Top-level module statements are extracted into a guarded initializer that executes once before any public export call.
- Retained module-scope bindings persist across subsequent invocations.
- Function declarations, expressions, and control flow structure into high-level intermediate representation.

### 3. Contract Resolution

Before SSA lowering, the compiler reconciles Perry HIR against the resolved WIT world:
- **Signatures**: Validates function signatures, argument counts, and return types. Verifies that functions containing suspension points (I/O, timers, streams) are declared `async func` in WIT.
- **Capability Plans**: Links native operations (filesystem, HTTP, clock, random, process) to canonical import signatures.
- **Canonical ABI Layouts**: Derives scratch buffer sizes, return area offsets, and variant tag layouts required by Canonical ABI lowering.

### 4. Typed MIR Lowering

Perry HIR lowers into typed SSA intermediate representation built on pliron:
- Every local variable initializes as a slot, converted into SSA values and block arguments via `mem2reg`.
- Branch joins and loop headers pass mutable local state through SSA block parameters.
- Exception unwinding uses shared catch and finally dispatch paths to guarantee consistent cleanup order.
- References to heap objects are tracked in root frames linked across asynchronous suspension points.

Function invocations, native capability calls, and Promise observations follow explicit ownership conventions:

- **Function Invocations**: Caller frames preserve root ownership of pointer arguments, while callees receive borrowed references and return fresh allocation ownership.
- **Capability Calls**: Outbound linear memory buffers pin across the host boundary. Opaque resource handles transfer through transactional descriptor scopes.
- **Promise Observations**: Settled asynchronous tasks retain a single outcome for repeated observation ([Liskov & Shrira 1988], §3). Observing continuations borrow the retained value without cloning its heap graph.

Runtime builders assemble scheduling hooks, combinators, scalar capabilities, process context, and date codecs. Referenced pure Rust guest helpers link into the module's shared memory.

#### Optimization

Pliron's `Passes` and `AnalysisManager` schedule verified transformations and cache analyses for each unchanged MIR snapshot. Mutating passes invalidate affected analyses. The optimizer first promotes locals to SSA, specializes WIT numeric domains, then runs canonicalization, SCCP, CFG simplification and DCE to convergence. Ownership rewrites follow this cleanup; append fast-path expansion runs once in its late phase.

| Responsibility | Pliron contribution | Perry contribution |
| --- | --- | --- |
| Arithmetic and comparison folding | SCCP propagation, materialization, DCE; `APInt` arithmetic | `ConstFoldInterface` semantics for scalar operations and conversions |
| Constant branches | SCCP reachability and CFG simplification | `BranchOpFoldInterface` for MIR terminators; unused block-argument adapter |
| Async frames, leases, GC roots, append liveness | Cached SSA liveness and dominance | Block-local live intervals from definition and last-use positions; deterministic frame layout, root selection, ownership transfer rules |
| Optional/conversion simplification, record decomposition, character scalarization, owned append, append fast paths | `MatchRewrite` traversal and `IRRewriter` mutation | Rewrite patterns and semantic predicates |
| Record promotion | `Mem2RegPass` | Record-to-slot decomposition |
| Successful-unwrap reuse | Cached dominance and rewriting | Exception-aware availability rules |
| Numeric and runtime facts | Integer types, cloning, analysis caching; Tarjan SCCs for CFG and call-graph cycles | Shared dependency worklist, context selection, range widening, parameter provenance, effect transfer and buffer-detachment rules |
| Leaf inlining and argument promotion | Cloning, rewriting, cached call effects | Legality and profitability rules |

WIT constraints enter before arithmetic folding. Cached `ProgramFacts` uses one dependency queue for numeric domains, ranges, result-width requirements, reference-parameter provenance and callee effects. Dependencies connect SSA values, CFG edges, synchronous and asynchronous calls, and every return result. Queue membership bits suppress duplicate work. Local effects are classified once per snapshot; changing a callee summary revisits dependent call sites. Function contexts are keyed by argument domains and result requirements. Finite domain choices, recursive context reuse and conservative range widening ensure termination. Representation rewriting fixes integer wrap points before algebraic optimization. Contexts that preserve representations and callee choices reuse the original body. The same snapshot supplies bounds-check facts. The [numeric semantics](README.md#semantics) define wrapping, widening, exact 64-bit values and floating-point boundaries.

##### Fact product lattice

`ProgramFacts` computes the least fixpoint over a reduced product lattice ([Cousot & Cousot 1979]) $\mathcal{L} = \mathcal{D}_{\text{num}} \times \mathcal{I}_{\text{range}} \times \mathcal{R}_{\text{req}} \times \mathcal{O}_{\text{out}} \times \mathcal{H}_{\text{heap}}$ across the interprocedural dependency graph:

1. **Bottom (`never` / $\bot$)**: Unreachable control-flow paths, diverging terminators, and unvisited SSA values evaluate at $\bot$ (`Fact::Bottom`, `Range::Bottom`, `Outcomes::Bottom`), satisfying $\bot \sqcup x = x$.
2. **Numeric Domain Lattice ($\mathcal{D}_{\text{num}}$)**:
   $$\bot < \mathcal{I} < \mathtt{Numeric}\;(\mathtt{f64})$$
   The integer sub-lattice $\mathcal{I} = \{ \mathtt{u8}, \mathtt{s8}, \mathtt{u16}, \mathtt{s16}, \mathtt{u32}, \mathtt{s32}, \mathtt{u64}, \mathtt{s64} \}$ is partially ordered by value-set inclusion (`Integer::common`):
   - **Same signedness**: $\mathtt{u8} \sqsubset \mathtt{u16} \sqsubset \mathtt{u32} \sqsubset \mathtt{u64}$ and $\mathtt{s8} \sqsubset \mathtt{s16} \sqsubset \mathtt{s32} \sqsubset \mathtt{s64}$ with join $\max(w_1, w_2)$.
   - **Mixed signedness ($\mathtt{u}w_u \sqcup \mathtt{s}w_s$)**: If $w_s > w_u$, the signed domain subsumes the unsigned range ($\mathtt{u8} \sqcup \mathtt{s16} = \mathtt{s16}$, $\mathtt{u16} \sqcup \mathtt{s32} = \mathtt{s32}$, $\mathtt{u32} \sqcup \mathtt{s64} = \mathtt{s64}$). If $w_s \le w_u < 64$, the join steps to the smallest signed width $w' > w_u$ in $\{8, 16, 32, 64\}$ ($\mathtt{u8} \sqcup \mathtt{s8} = \mathtt{s16}$, $\mathtt{u16} \sqcup \mathtt{s16} = \mathtt{s32}$, $\mathtt{u32} \sqcup \mathtt{s32} = \mathtt{s64}$). At 64 bits, $\mathtt{u64} \sqcup \mathtt{s64} = \mathtt{u64}$.
   - Joining any integer domain with $\mathtt{Numeric}$ yields $\mathtt{Numeric}$ ($\top_{\text{num}}$).
3. **Backward Demand Lattice ($\mathcal{R}_{\text{req}}$)**:
   $$\mathtt{Inferred}\;(\text{unconstrained}) < \mathsf{Integer}(\mathcal{I}) < \mathtt{Original}\;(\top_{\text{req}})$$
   Backward demand propagates from WIT export signatures, explicit type annotations, array indices, and bitwise operations. When integer arithmetic lacks a narrower requirement, `width_defaults` promotes its evaluation domain via `integer.unconstrained()` to native WebAssembly `i64` ($\mathtt{s64}$ or $\mathtt{u64}$), avoiding premature overflow in 64-bit registers without heap allocation ([Stephenson et al. 2000]).
4. **Interval Range Lattice ($\mathcal{I}_{\text{range}}$)**:
   $$\bot_{\text{range}} < [l, u] \subseteq [-2^{63}, 2^{64}-1] < \top_{\text{range}}$$
   Unstable interval bounds widen eagerly on repeated expansion (`expansions >= 1`) directly to domain boundaries ([Cousot & Cousot 1977], [Rodrigues et al. 2013]). Staged landmark widening through bitwidth thresholds is planned to refine loop induction ranges before reaching domain extremes.
5. **Heap Facts ($\mathcal{H}_{\text{heap}}$)**: Record fields and array element summaries propagate `(Fact, Range)` through heap cells across bounded allocation-site alias sets ($\le 32$ sites before collapsing to a per-type summary cell). Optional values propagate payload facts directly, while nullish absence modes (`Undefined`, `Null`, `Both`) are verified structurally. A dedicated presence dataflow lattice ($\bot < \{\mathsf{Present}, \mathsf{Absent}\} < \top$) is planned to track narrowing across nullish condition branches.
6. **Promise Settlement Outcomes ($\mathcal{O}_{\text{out}}$)**: The 4-state `Outcomes` lattice (`Bottom`, `Fulfilled`, `Rejected`, `Either`) tracks promise settlement status across await points and combinators.
7. **Context Specialization**: At most 32 precise `(arg_domains, required_result)` contexts are specialized per function before subsequent call sites join monotonically into a summarized context.

Aggregate allocation identities and stable slot ranges participate in the same worklist before decomposition. Record fields, array element summaries, block arguments, retained calls and return values preserve aliases. Writes join into their selected slots, including writes reached through aliases; unrelated record fields retain their facts. Array extension copies element facts while preserving references to shared elements. Unknown storage or calls invalidate reachable objects, and recursive source schemas use finite reserved summary identities. Each alias set holds at most 32 allocation sites before collapsing to a typed summary; the optimizer reports these fallbacks. Collapse also invalidates existing aliases, with a one-time cost proportional to the affected objects. Heap operations and field indices are classified once per MIR snapshot. Declared ABI and carrier layouts govern storage types and representation boundaries.

Array identities carry length bounds and subscribe their readers to the same queue. Dense literals establish an initialized prefix. Writes inside that prefix preserve the exact length; possible appends and extensions retain the minimum length while widening the maximum. Unknown views or calls remove precise bounds. These are stable bounds across all executions of an allocation site, not lengths at individual program points. The bounds pass borrows the facts to replace fixed lengths with constants and proven reads with unchecked projections. Canonicalization and pliron SCCP/DCE eliminate redundant bounds checks. Byte-buffer detachment retains separate legality checks.

Runtime summaries retain allocation, collection, suspension, reentry, record mutation, escape and failure information. Reference-parameter provenance follows SSA uses and branch arguments in the shared queue. Async rejection remains distinct from synchronous throwing. Promise settlement alternatives propagate through calls, joins and aggregate slots in the same queue; unknown sources remain conservative. Status conventions and escaping exceptions are separate facts, so a caught failure retains its internal status without requiring an export error channel. Proven fulfilled awaits omit rejection branches; possible pending host work retains the existing unhandled-await reporting path and cancellation. Allocation reports, argument promotion, lifetime assignment, GC roots and export scopes borrow effect projections from `ProgramFacts`. Capability transfers copy fixed-size flags; escape relations grow in place. Boundary scopes account for initializers and canonical marshalling before lowering inserts runtime calls. Runtime frame constructors register their resume handlers; the dispatcher emits calls to those handlers. Async frame slots are cleared at collection and suspension points. Lifetime attributes preserve cached control-flow and value analyses. Repeated-allocation facts distinguish call-graph recursion from the location of allocations: pliron SCCs identify recursive functions and cyclic CFG blocks, and the shared queue propagates allocation effects at those call sites. An allocation before a non-allocating loop or recursive call remains bounded. Bounded synchronous tasks use regions for records, arrays, variants, frame storage and temporary reference lists. Repeated allocations, unbounded helpers, suspension, reentry, persistent initialization and resource-bearing boundaries retain collection. Checked builds also retain collection. Collector roots are eliminated directly at producers during lowering.

#### JSON construction

An expected JSON type selects a generated MIR decoder for numbers, exact 64-bit integers (`s64`/`u64`), booleans, strings, numeric/string unions, optional values, records, arrays and tuples. Recursive schemas reserve decoder identities before constructing their bodies. The decoder writes known fields directly into native storage and retains unknown fields in JSON residue. Dynamic JSON and schemas requiring snapshot conversion use the shared graph destination. When a typed `JSON.parse<T>(JSON.stringify(x))` round-trip operates on a statically known source shape without unknown JSON residue, `NormalizeJsonCopyPass` rewrites it directly into an allocating, schema-normalizing MIR deep copy (`NaN`/`±Infinity` to `null` or `SyntaxError`, `-0.0` to `+0.0`, `undefined` elements to `null`, and dropped omitted fields) without serializing or reparsing text.

Typed construction and dynamic parsing share the JSON lexer, Unicode-escape validation and number conversion. Runtime strings carry validated immutable UTF-8; both parser destinations preserve that contract instead of rescanning it. Raw byte adapters validate UTF-8 before entering the shared lexer. Number tokens collect decimal facts during lexical validation; conversion retains correctly rounded IEEE-754 behavior, while `s64` and `u64` targets parse and format exact 64-bit signed and unsigned integer tokens directly.

A decoder carries its lookahead through SSA and returns a value plus a deferred schema-failure flag. Repeated object keys replace both the candidate value and its schema result. Syntax errors take precedence at completion; malformed input unwinds bounded container state while partial values remain unpublished. Parsing preserves the depth limit of 128, nullable/absent distinctions and independent mutable views of immutable snapshots.

Generated decoders form a verified synchronous construction group. Every direct callee belongs to that group, and its operations complete without collection, suspension, callbacks or conservative allocation. Collection and root publication occur at the caller; allocation, escape, task and failure effects remain visible to the shared analysis. Repeated internal GC checks and root setup are omitted under this proof.

### 5. Code Generation

The code generator translates typed MIR into WebAssembly instructions:

- Reducible control flow structures into valid WebAssembly blocks, loops, and branches using Ramsey's dominator-tree algorithm ([Ramsey 2022]).
- **Operand Stack Scheduling**: Single-use values defined in the same block as their user fold into expression trees evaluated on the operand stack when effect and trap ordering permits. Constants rematerialize at use sites. Other live values and used block parameters occupy WebAssembly locals, with locals reused across non-overlapping live ranges.
- **Canonical ABI Marshalling**: Synthesizes boundary stubs directly around compiled entry points according to the [Canonical ABI]. Inbound values lift from flat registers into typed guest objects, while outbound values lower into caller return areas.

Pliron CFG simplification and DCE run on the lowered Wasm IR to convergence before control-flow structurization.

The resulting core WebAssembly module can be emitted directly for low-level embedding or unit testing.

### 6. Component Encoding

The component wrapper binds native WASI 0.3 imports, generates canonical ABI adapters, embeds resolved WIT metadata, and invokes the component encoder to emit a validated WASI 0.3 component.

Interface glue implements the [Canonical ABI] around exported entry points:

- **Synchronous Exports**: Emits flat Canonical ABI entry points, lifts arguments, invokes the MIR body and lowers results. Task scopes provide `cabi_post_<export>` for cleanup; independent scalar scopes call the implementation directly.
- **Asynchronous Exports**: Synthesizes the stackless WASI 0.3 callback trio: `[async-lift]<export>`, `[callback][async-lift]<export>`, and `finish_<export>`.
- **Memory Allocation**: Exposes `cabi_realloc` when export lowering or a reachable canonical import requires host allocation. The export binds directly to the linked guest allocator. Surviving memory operations, data and helpers determine whether the core module needs memory.

## Memory

Perry-WIT manages all heap-allocated objects in a single, non-moving guest memory space managed by `talc`.

### Text
Strings are stored as validated UTF-8 byte sequences using Unicode scalar indexing. Lone surrogates and UTF-16 code-unit operations are rejected at compile time. Canonical ABI string inputs carry valid UTF-8. `TextDecoder` replaces malformed byte sequences by default and rejects them in fatal mode. JSON validates its grammar and Unicode escapes against these established string invariants. Binary payloads remain intact as raw byte views without intermediate transcoding. Repeated appends reuse spare buffer capacity for amortized linear copying.

### Heap
Records, dictionaries, finite unions, JSON trees, byte views, and Promise outcomes share a unified managed heap.

Linear memory partitions into explicit lifetime tiers:
- **Static**: Initialized data segments, embedded helper code, and immutable string constants.
- **Rooted**: Dynamic guest objects, records, and arrays managed via size-class pages, mark-sweep tracing, and root descriptors.
- **Pinned**: In-flight host I/O buffers and Canonical ABI scratch areas held until host acknowledgement.
- **Ephemeral**: Stack-allocated scalar frames and immediate operands.

Loop-level garbage collection traces active root frames and coalesces dead allocations. Canonical import scratch buffers use dedicated root scopes, keeping buffers valid while concurrent tasks allocate and collect.

Canonical values follow a schema-guided ownership model for outgoing arguments, incoming results, and exported results. The collector traverses the WIT type layout across strings, byte buffers, nested lists, records, tuples, options, results, and variants. Incoming return areas are initialized and rooted before the host populates them. Outgoing graphs are rooted before adapters suspend or transfer ownership. A return-area address alone does not imply ownership of referenced allocations.

Export adapters manage the lifecycle of both the returned source graph and canonical scratch storage. The export adapter transfers ownership to the callback, ensuring raw ABI values never outlive their owner. Storage is released only after `task.return` consumes the result, or after all operations acknowledge cancellation. Subsequent invocations verify that ownership slots are empty before dispatching new calls.

### Scopes
Component instances isolate runtime state within dedicated `RuntimeInstance` contexts. Each instance owns its active tasks, root frame descriptors, resource tables, and process context.

Module-level bindings and cached process context persist for the lifetime of the component instance. Per-call temporary objects are reclaimed by the canonical post-return hook after results transfer to the caller. Instances can alternate on a single thread or execute concurrently across threads. Dropping an instance cleanly releases all associated guest allocations and active tasks.

Pure Rust guest helpers validate buffer boundaries before borrowing guest memory. Their minimal unsafe boundaries remain isolated from pure logic and introduce no auxiliary heap allocators.

## Concurrency

Asynchronous TypeScript functions execute eagerly until they reach an initial suspension point.

### Execution
Completed tasks store their settlement state and outcome value for repeated observation.

Continuations are scheduled directly through native WASI 0.3 wakeups. The runtime does not maintain a secondary event loop or microtask queue.
Awaits on already-settled promises advance inline on the stackless state machine. Pending observers of the same promise share a single native completion whose callback resumes registered frames in registration order. The host runtime determines the scheduling order across distinct completion events.

### Settlement
Guest task records maintain explicit settlement states, observer lists, and outcome values in the guest heap. Combinator operations (`all`, `allSettled`, `race`) register as observers on operand tasks and propagate settlements deterministically when dependencies resolve.

Observer links live inside suspended frames or a combinator's input storage. A task list retains pending registrations for garbage collection and cancellation, while a handle registry retains native completion owners. Runnable dispatch is driven directly by native host events. Combinators count input settlements and signal once their result is ready, ensuring long dependency chains advance through native events with bounded guest stack usage.

Non-winning branches in a race continue running as background tasks. The runtime verifies that all spawned tasks settle or acknowledge cancellation before the top-level export returns. Attempting to return while unresolved background work remains results in a runtime trap.

### Streams
Readable stream readers decouple public read promises from underlying native transfer completion:
- Releasing a reader lock rejects pending read promises while preserving native buffer state. Subsequent readers receive previously buffered chunks.
- Explicit cancellation retains the native transfer owner until host acknowledgement arrives.
- Request and response body consumption methods pull data incrementally through this same streaming pipeline.
- Asynchronous iteration loops (`for await`) over byte streams lower into locked reader calls with structured cleanup. Early loop termination explicitly cancels the stream and releases the lock, while loop exceptions take precedence over cancellation errors.

### Host Resource Integration

Host resources—open file descriptors, network sockets, timers—are tracked via host resource tables. The guest runtime maintains valid handles across asynchronous suspensions. Cancellation retains in-flight operation owners until the host acknowledges completion; integration tests verify that transfer buffers, completion futures, and resource-table entries are released across repeated calls.

In the Wasmtime hosts used for verification, a [Store](https://docs.wasmtime.dev/api/wasmtime/struct.Store.html) owns instance state and host data, including the resource table. An unhandled trap exits guest execution without running source-level `finally` handlers. Store disposal is a separate lifecycle step: dropping the store releases its owned instance state and host data, with resource cleanup determined by the destructors of the host objects it owns.

## Linked Helpers

Complex algorithmic operations—including JSON serialization, date calculations, text transformations, and HTTP header decoding—are implemented in freestanding Rust helper crates.

### Bytecode Embedding
Helper libraries compile to standalone `wasm32-unknown-unknown` binaries without allocators or system runtimes. The resulting bytecode artifacts embed directly into the compiler binary during build time, removing any runtime dependency on Rust or LLVM.

### Capability Pruning
The linker computes transitive function reachability from surviving core calls and required ABI exports. Reachable canonical imports contribute allocator roots when their ABI requires host allocation. Ordinary calls carry helper dependencies; unreferenced functions are pruned. Helper data retains each library's private placement. Constant address stores and zero fills are evaluated in initialization order before function indices are assigned. Fully evaluated initialization sequences lose their startup roots; ordinary calls and table references still retain callable functions. Unsupported instructions or writes outside the supported private segments preserve the complete runtime initialization sequence.

### Static Relocation
During code generation, the compiler relocates helper functions, signatures, and element tables directly into the target WebAssembly module. Instructions are re-encoded to target local function indices, allowing pure helper routines to execute with zero call overhead.

### Memory Placement
Static data segments required by helper routines are placed contiguously above user static data within the guest module's primary linear memory. When a helper requires call-frame scratch space, it shares a bounded stack reservation with the guest instance. This preserves the single-heap ownership model without introducing auxiliary memory allocators.

## Verification

```text
  Ordinary TypeScript ──────────── Node execution
          │                            │
          │       compare supported    │
          │       source behavior      │
          ▼                            │
     Perry compiler                    │
          │                            │
          ▼                            │
      Component ────────────── observable results
          │
          ▼
  Controlled Wasmtime host
  • exact imports/exports and canonical types
  • gated concurrency, cancellation, and completion
  • native resource tables and memory limits
```

Verification rests on two complementary boundaries:

1. **Source Equivalence**: Conformance fixtures run under both Node.js and compiled WASI 0.3 components. These checks compare evaluation order, lexical scope, runtime values, exception unwinding, and standard library behaviors.
2. **Host Integration**: Controlled Wasmtime hosts verify Canonical ABI marshalling, stackless coroutine suspension, cancellation signal propagation, fuel scaling bounds, and deterministic resource cleanup across serial invocations.

The benchmark harness compares every unchanged-semantics workload against the checked-in baseline. Named core size, stripped component size, guest fuel, initial memory and peak memory must each stay within 105% of their baseline; zero baselines remain zero. Input hashes and locked Wasmtime versions are verified before comparison. Allocation counters, function and helper counts, specialization and convergence statistics, compilation time and execution time appear in the JSON report. Timing is informational. Linux CI runs the comparison using its existing test build and uploads the report.

The scalar artifact test inspects named and stripped Wasm: pure `s32` addition is one directly exported function containing only two argument loads and `i32.add`. It has no forwarding call, memory, runtime helper, allocator or cleanup export. Signed 8- and 16-bit boundaries use sign-extension instructions; producer range guarantees suppress redundant normalization. Generated stubs that only forward arguments and results contribute an export name on the implementation. Marshalling and stateful stubs retain their boundary behavior. Native/Wasm library and CLI parity checks cover every WIT integer width, widening, exact 64-bit arithmetic and unconstrained raw compilation.

## References

- **[Cousot & Cousot 1977]**: P. Cousot, R. Cousot. *Abstract Interpretation: A Unified Lattice Model for Static Analysis of Programs by Construction or Approximation of Fixpoints*. ACM POPL, pp. 238–252, 1977.
- **[Cousot & Cousot 1979]**: P. Cousot, R. Cousot. *Systematic Design of Program Analysis Frameworks*. ACM POPL, pp. 269–282, 1979.
- **[Liskov & Shrira 1988]**: B. Liskov, L. Shrira. *Promises: Linguistic Support for Efficient Asynchronous Procedure Calls in Distributed Systems*. ACM PLDI, pp. 260–267, 1988.
- **[Stephenson et al. 2000]**: M. Stephenson, J. Babb, S. Amarasinghe. *Bitwidth Analysis with Application to Silicon Compilation*. ACM PLDI, pp. 108–120, 2000.
- **[Rodrigues et al. 2013]**: R. E. Rodrigues, V. H. S. Campos, F. M. Q. Pereira. *A Fast and Low-Overhead Technique to Secure Programs Against Integer Overflows*. IEEE/ACM CGO, 2013.
- **[Ramsey 2022]**: N. Ramsey. *Beyond Relooper: Recursive Translation of Unstructured Control Flow to Structured Control Flow (Functional Pearl)*. Proceedings of the ACM on Programming Languages 6(ICFP), 2022.
- **[Canonical ABI]**: WebAssembly Community Group. *Canonical ABI Explainer*. WebAssembly/component-model, living design document.

[Cousot & Cousot 1977]: https://doi.org/10.1145/512950.512973
[Cousot & Cousot 1979]: https://doi.org/10.1145/567752.567778
[Liskov & Shrira 1988]: https://doi.org/10.1145/53990.54016
[Stephenson et al. 2000]: https://doi.org/10.1145/349299.349317
[Rodrigues et al. 2013]: https://doi.org/10.1109/CGO.2013.6494993
[Ramsey 2022]: https://doi.org/10.1145/3547621
[Canonical ABI]: https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md
