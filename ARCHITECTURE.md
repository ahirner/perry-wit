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

### 5. Code Generation

The code generator translates typed MIR into WebAssembly instructions:

- Reducible control flow structures into valid WebAssembly blocks, loops, and branches using Ramsey's dominator-tree algorithm ([Ramsey 2022]).
- **Operand Stack Scheduling**: Single-use values defined in the same block as their user fold into expression trees evaluated on the operand stack when effect and trap ordering permits. Constants rematerialize at use sites. Other live values and used block parameters occupy WebAssembly locals, with locals reused across non-overlapping live ranges.
- **Canonical ABI Marshalling**: Synthesizes boundary stubs directly around compiled entry points according to the [Canonical ABI]. Inbound values lift from flat registers into typed guest objects, while outbound values lower into caller return areas.

The resulting core WebAssembly module can be emitted directly for low-level embedding or unit testing.

### 6. Component Encoding

The component wrapper binds native WASI 0.3 imports, generates canonical ABI adapters, embeds resolved WIT metadata, and invokes the component encoder to emit a validated WASI 0.3 component.

Interface glue implements the [Canonical ABI] around exported entry points:

- **Synchronous Exports**: Emits flat Canonical ABI entry points, lifts arguments, invokes the MIR body, lowers results, and provides `cabi_post_<export>`.
- **Asynchronous Exports**: Synthesizes the stackless WASI 0.3 callback trio: `[async-lift]<export>`, `[callback][async-lift]<export>`, and `finish_<export>`.
- **Memory Allocation**: Exposes `cabi_realloc`, binding Canonical ABI dynamic allocations directly to the guest `talc` allocator.

## Memory

Perry-WIT manages all heap-allocated objects in a single, non-moving guest memory space managed by `talc`.

### Text
Strings are stored as validated UTF-8 byte sequences using Unicode scalar indexing. Lone surrogates and UTF-16 code-unit operations are rejected at compile time. External text entering through Canonical ABI boundaries, JSON, or `TextDecoder` undergoes strict UTF-8 validation. Binary payloads remain intact as raw byte views without intermediate transcoding. Repeated appends reuse spare buffer capacity for amortized linear copying.

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
Awaits on already-settled promises advance inline on the stackless state machine. Pending observers of the same promise share one native completion. Its callback resumes those frames in registration order. P3 selects the order between completion events; exact Node microtask interleaving is not guaranteed.

### Settlement
Guest task records maintain explicit settlement states, observer lists, and outcome values in the guest heap. Combinator operations (`all`, `allSettled`, `race`) register as observers on operand tasks and propagate settlements deterministically when dependencies resolve.

Observer links live inside suspended frames or a combinator's input storage. A task list retains pending registrations for GC and cancellation; a handle registry retains native completion owners. Neither structure orders runnable work. Combinators count input settlements and signal once their result is ready, so long dependency chains advance through native events with bounded guest stack usage.

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
When compiling a user component, the compiler scans AST capability requirements to identify only the helper functions actually referenced by the program. Unreferenced helper functions and unused data sections are pruned completely.

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

## References

- **[Liskov & Shrira 1988]**: B. Liskov, L. Shrira. *Promises: Linguistic Support for Efficient Asynchronous Procedure Calls in Distributed Systems*. ACM PLDI, pp. 260–267, 1988.
- **[Ramsey 2022]**: N. Ramsey. *Beyond Relooper: Recursive Translation of Unstructured Control Flow to Structured Control Flow (Functional Pearl)*. Proceedings of the ACM on Programming Languages 6(ICFP), 2022.
- **[Canonical ABI]**: WebAssembly Community Group. *Canonical ABI Explainer*. WebAssembly/component-model, living design document.

[Liskov & Shrira 1988]: https://doi.org/10.1145/53990.54016
[Ramsey 2022]: https://doi.org/10.1145/3547621
[Canonical ABI]: https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md
