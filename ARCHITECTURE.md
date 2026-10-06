# Perry-WIT Architecture

Perry-WIT compiles static TypeScript directly to WebAssembly components targeting WASI 0.3 (Preview 3) through Perry HIR and WAFFLE SSA.

### Unified Pipeline

All compiler targets—the CLI, public Rust library API, Nix derivations, and integration test suites—converge on the exact same lowering pipeline and resolved-WIT component encoder. Tests exercise the identical encoding and validation path used in production.

## Principles

1. **Ahead-of-Time Execution**
   TypeScript operations compile directly into native WebAssembly instructions. Primitive local variables map to SSA values and block parameters. Compound aggregates and retained objects live in managed guest memory.

2. **Static Source Contracts**
   Binding-aware validation rejects unsupported dynamic JavaScript patterns while the AST retains full contextual information for precise diagnostics. Lightweight runtime checks handle data-dependent array bounds and finite variant tags.

3. **Direct HIR-to-SSA Lowering**
   Control flow constructs (branches, loops, exception paths, function calls, and `await` points) share a unified lowering and validation pipeline. Typed capability plans link source validation directly to canonical imports and runtime signatures.

4. **Native Concurrency**
   Suspension, resumption, and asynchronous transfers are managed by the host via WASI 0.3 Component Model primitives. Guest task records and observer lists maintain Promise identity, repeated observation, and combinator settlements. Continuations are scheduled directly by native P3 wakeups.

5. **Authoritative WIT**
   Pinned official WASI definitions specify built-in platform capabilities, while application WIT files define internal interfaces. The SDK and compiler share unified export conventions, ensuring helper names don't leak into host protocols.

6. **Unified Guest Heap**
   Pure algorithms rely on compact Rust helpers compiled directly into the guest module. Dynamic allocations, root frames, canonical scratch buffers, and suspended task states follow an explicit, single-heap ownership model.

7. **Reproducible Evidence**
   Nix flakes pin all toolchain versions, compilers, and interface definitions. Differential testing against Node.js establishes source-level equivalence. Controlled WASI 0.3 hosts verify canonical ABI compliance, stackful suspension, resource lifetimes, and cleanup.

WAFFLE serves as the primary compiler backend. The pinned Rust toolchain compiles allocation-free Wasm helpers directly into the guest module.

## Pipeline

```text
 TypeScript entry + static ESM dependencies       Application + official WIT
                     │                                      │
          Parse and resolve bindings               Resolve packages/world
                     │                                      │
       Validate source forms and normalize capabilities     │
                     │                                      │
                 Perry HIR ───── check signatures/effects ──┘
                     │
          Lower into typed WAFFLE SSA
          + canonical ABI adapters
          + guest runtime and Rust helpers
                     │
       Validate/optimize and emit core Wasm
                     │
           Embed resolved WIT metadata
                     │
       ComponentEncoder → validated component
```

### 1. Source Analysis

The compiler resolves local static ESM dependencies and named re-exports. Lexical binding analysis distinguishes global built-ins from shadowed local identifiers.

Before lowering to Perry HIR, source validation normalizes supported expressions. Static checks enforce a read-only execution context, static type constraints, and valid `Date` and `Temporal` forms. Evaluation order for function arguments and object receivers is strictly preserved.

### 2. Contract Resolution

The compiler checks source function signatures against WIT world declarations and reachable host effects. Package resolution handles versioned dependencies, giving local WIT definitions precedence and rejecting ambiguous world targets.

Module initialization supports both scripts and export libraries:
- When targeting a world exporting `wasi:cli/run@0.3.0`, the compiler synthesizes a CLI command adapter.
- A single source module can simultaneously serve as a CLI entry point and an export library.
- Top-level module statements are extracted into a guarded initializer that executes once before any public export call.
- Retained module-scope bindings persist across subsequent invocations.

### 3. SSA Lowering

Perry HIR lowers into typed WAFFLE SSA basic blocks:
- Branch joins and loop headers pass mutable local state through SSA block parameters.
- Exception unwinding uses shared catch and finally dispatch paths to guarantee consistent cleanup order.
- Function invocations, native capability calls, and Promise observations adhere to uniform ownership rules.
- References to heap objects are tracked in root frames that remain linked across asynchronous suspension points.

Runtime builders assemble scheduling hooks, combinators, scalar capabilities, process context, and date helpers. Pure algorithmic tasks (such as JSON parsing and date arithmetic) are built from compiled Rust helpers and linked into the module's shared memory.

### 4. Code Generation

WAFFLE validates SSA form, enforces reducible control flow, applies graph-level optimizations, and reconstructs structured WebAssembly.

The resulting core Wasm module can be emitted directly for low-level embedding or unit testing. Canonical scratch buffers and return value layouts are derived directly from WIT interface types.

### 5. Component Encoding

Component wrapping generates canonical import and export adapters and wires native WASI 0.3 capabilities.

Finally, the encoder embeds the resolved WIT world metadata and delegates to `wit_component::ComponentEncoder` to produce a validated WASI 0.3 component.

## Conformance

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
2. **Host Integration**: Controlled Wasmtime hosts verify Canonical ABI marshalling, stackful coroutine suspension, cancellation signal propagation, and deterministic resource cleanup across serial invocations.

## Memory

Perry-WIT manages all heap-allocated objects in a single, non-moving guest memory space.

### Text
Strings are stored as validated UTF-8 byte sequences using Unicode scalar indexing. Lone surrogates and UTF-16 code-unit operations are rejected at compile time. External text entering through Canonical ABI boundaries, JSON, or `TextDecoder` undergoes strict validation, while binary payloads remain intact as raw byte views.

### Heap
Records, dictionaries, finite unions, JSON trees, byte views, and Promise outcomes share a unified managed heap.

Runtime tags differentiate dynamic representations when static types alone do not provide enough specificity. Loop-level garbage collection traces active root frames and coalesces dead allocations. Canonical import scratch buffers use dedicated root scopes, keeping buffers valid while sibling tasks allocate and collect.

Canonical values use one schema-guided ownership rule for outgoing arguments, incoming results, and exported results. The collector follows the WIT layout through strings, byte buffers, nested lists, records, tuples, options, results, and variants. Incoming return areas are initialized and owned before the host fills them; outgoing graphs are owned before the adapter can suspend or transfer them. This replaces string-list-specific backing-storage roots. A return-area address alone does not own the allocations referenced by that area.

`PendingExportResult` owns both the returned source graph and canonical scratch storage. The earlier adapter-local result root is part of this same scope. The export adapter transfers its retained scope to the callback; the worker's raw ABI values never outlive that owner. Publication releases the owner only after `task.return` consumes the result. Cancellation uses the same release path after all operations acknowledge cancellation. Entry checks require an empty ownership slot before the next call.

Regression probes force collection and poison reclaimed storage at worker and callback handoffs. They exercise freshly constructed and imported graphs, text and bytes, nested aggregate results, repeated calls under a memory limit, and cancellation followed by successful reuse.

### Scopes
Module-level bindings and cached process context persist for the lifetime of the component instance. Per-call temporary objects are reclaimed by the canonical post-return hook after results have been transferred to the caller.

Pure Rust guest helpers (such as JSON and date codecs) validate buffer boundaries before borrowing guest memory. Their minimal unsafe boundaries are isolated from pure logic and introduce no auxiliary heap allocators.

## Concurrency

Asynchronous TypeScript functions execute eagerly until they reach an initial suspension point.

### Execution
Completed tasks store their settlement state and outcome value for repeated observation.

Continuations are scheduled directly through native WASI 0.3 wakeups. The runtime does not maintain a secondary event loop or microtask queue.
Awaits on already-settled promises yield immediately through native wakeups without allocating observer nodes.

### Combinators
Promise combinators provide deterministic concurrency handling:
- `Promise.all` preserves input order and rejects immediately upon the first failure.
- `Promise.allSettled` records the outcome of every input promise.
- `Promise.race` settles with the earliest observed result.
- Empty input arrays resolve immediately for `all` and `allSettled`, while an empty `race` remains pending.

Non-winning branches in a `Promise.race` continue running in the background. They must settle or be cancelled before the top-level invocation finishes. Attempting to return from an export while ordinary work remains unresolved results in a runtime trap.

### Streams
Readable stream readers decouple public read promises from underlying native transfer completion:
- Releasing a reader lock rejects pending read promises while preserving native buffer state. Subsequent readers receive any previously buffered chunks.
- Explicit cancellation retains the native transfer owner until host acknowledgement is received.
- Request and response body consumption methods (`text()`, `json()`, `bytes()`, `arrayBuffer()`) pull data incrementally through this same streaming pipeline.
- `for await` loops over byte streams lower into locked reader calls with structured cleanup. Early loop termination explicitly cancels the stream and releases the lock, while loop exceptions take precedence over cancellation errors.

### Resource Management
When a guest component accesses external resources—such as files, network sockets, or timers—the host maintains their state across asynchronous suspensions.

The runtime keeps these resources alive during in-flight operations and releases them automatically when an operation completes or when an `AbortSignal` triggers cancellation. Unhandled traps or abnormal termination cause the host to release all associated resources during store disposal without running guest `finally` blocks.

## Helpers

Complex algorithmic operations—including JSON serialization, date and time calculations, text transformations, and HTTP header decoding—are implemented as pure Rust helper libraries.

### Bytecode Embedding
Helper libraries are compiled to standalone `wasm32-unknown-unknown` binaries without allocators. The resulting bytecode artifacts are embedded directly into the compiler binary during build time, removing any runtime dependency on Rust or LLVM.

### Pruning
When compiling a user component, the compiler scans AST capability requirements to identify only the helper functions actually referenced by the program. Unreferenced helper functions and dead data sections are pruned completely.

### Static Relocation and Linking
During SSA code generation, the compiler relocates helper functions, signatures, and element tables directly into the target WebAssembly module. Instructions are re-encoded to target local function indices, allowing pure helper routines to execute with zero call overhead.

### Unified Memory Placement
Static data segments required by helper routines are placed contiguously above user static data within the guest module's primary linear memory. When a helper requires call-frame scratch space, it shares a bounded stack reservation with the guest instance. This preserves the single-heap ownership model without introducing auxiliary memory allocators.

## Subsystems

The compiler layers cooperate to transform high-level TypeScript into validated components:

```text
┌─────────────────────────────────────────────────────────────┐
│                     Compiler Frontend                       │
│           ESM Loader  •  Source AST  •  Validation          │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                     Contract Resolution                     │
│    Signature Checks  •  WIT World Match  •  Effect Plans    │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                     Perry HIR Lowering                      │
│    Command Adapter  •  Guarded Initializer  •  Scopes       │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                      WAFFLE SSA Engine                      │
│   Block Joins  •  Exception Paths  •  Root Frame Tracking   │
│                                                             │
│   ┌───────────────────────┐       ┌───────────────────────┐ │
│   │   Runtime Builders    │       │  Linked Guest Helpers │ │
│   │    Task Schedulers    │       │ Pure Rust Wasm (JSON, │ │
│   │ Stream Readers & Locks│◄──────┤ Time, Text, Search)   │ │
│   │ HTTP & Filesystem I/O │       │ (Relocated Bytecode)  │ │
│   └───────────────────────┘       └───────────────────────┘ │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                 Wasm Optimization & Recovery                │
│      Graph Optimization  •  Reducible Control Flow          │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                  Component Model Encoding                   │
│    Canonical ABI Adapters  •  Resolved WIT  •  Encoder      │
└─────────────────────────────────────────────────────────────┘
```
