# Perry-WIT Architecture

Perry-WIT compiles static TypeScript directly to WebAssembly components targeting WASI 0.3 (Preview 3) through Perry HIR and WAFFLE SSA.

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
│                      WAFFLE SSA Engine                      │
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
│      Graph Optimization  •  Reducible Control Flow          │
└──────────────────────────────┬──────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────┐
│                  Component Model Encoding                   │
│    Canonical ABI Adapters  •  Resolved WIT  •  Encoder      │
└─────────────────────────────────────────────────────────────┘
```

### 1. Source Analysis

The frontend resolves local static ESM dependencies and validates TypeScript AST constructs. Lexical binding analysis distinguishes global built-ins from shadowed local identifiers, while static validation enforces read-only execution constraints and preserves operand evaluation order.

Before lowering to Perry HIR, the compiler interacts with the resolved WIT world:
- **Binding WIT Imports**: `bind_source` resolves Component Model import specifiers (`import { ... } from "pkg:interface"`) against the WIT world and rewrites AST bindings before standard import syntax is erased.
- **Command Adapter Synthesis**: If targeting a world exporting `wasi:cli/run@0.3.0` and no explicit runner function exists, `prepare_command` injects `export function __perry_command()` into the AST to wrap top-level statements.
- **AST Contract Checks**: Validates that source export declarations match the shapes required by the target world.

### 2. Perry HIR Lowering

The normalized AST lowers into Perry HIR:
- Top-level module statements are extracted into a guarded initializer (`extract`) that executes once before any public export call.
- Retained module-scope bindings persist across subsequent invocations.
- Function declarations, expressions, and control flow are structured into high-level intermediate representation.

### 3. Contract Resolution

Before SSA lowering, the compiler reconciles Perry HIR against the resolved WIT world:
- **Signatures**: Validates function signatures, argument counts, and return types. Verifies that functions containing suspension points (I/O, timers, streams) are declared `async func` in WIT.
- **Capability Plans**: Links native operations (filesystem, HTTP, clock, random, process) to canonical import signatures.
- **Canonical ABI Layouts**: Derives scratch buffer sizes, return area offsets, and variant tag layouts required by Canonical ABI lowering.

### 4. WAFFLE SSA Lowering

Perry HIR lowers into typed WAFFLE SSA basic blocks:
- Branch joins and loop headers pass mutable local state through SSA block parameters.
- Exception unwinding uses shared catch and finally dispatch paths to guarantee consistent cleanup order.
- Function invocations, native capability calls, and Promise observations adhere to uniform ownership rules.
- References to heap objects are tracked in root frames that remain linked across asynchronous suspension points.

Runtime builders assemble scheduling hooks, combinators, scalar capabilities, process context, and date codecs. Referenced pure Rust guest helpers are linked into the module's shared memory.

### 5. Code Generation

WAFFLE validates SSA form, enforces reducible control flow, applies graph-level optimizations, and reconstructs structured WebAssembly instructions.

The resulting core Wasm module can be emitted directly for low-level embedding or unit testing.

### 6. Component Encoding

The component wrapper binds native WASI 0.3 imports, generates canonical ABI import and export adapters, embeds the resolved WIT world metadata, and invokes `wit_component::ComponentEncoder` to emit a validated WASI 0.3 component.

## Memory

Perry-WIT manages all heap-allocated objects in a single, non-moving guest memory space.

### Text
Strings are stored as validated UTF-8 byte sequences using Unicode scalar indexing. Lone surrogates and UTF-16 code-unit operations are rejected at compile time. External text entering through Canonical ABI boundaries, JSON, or `TextDecoder` undergoes strict validation, while binary payloads remain intact as raw byte views.

### Heap
Records, dictionaries, finite unions, JSON trees, byte views, and Promise outcomes share a unified managed heap.

Runtime tags differentiate dynamic representations when static types alone do not provide enough specificity. Loop-level garbage collection traces active root frames and coalesces dead allocations. Canonical import scratch buffers use dedicated root scopes, keeping buffers valid while sibling tasks allocate and collect.

Canonical values follow a schema-guided ownership model for outgoing arguments, incoming results, and exported results. The collector traverses the WIT type layout across strings, byte buffers, nested lists, records, tuples, options, results, and variants. Incoming return areas are initialized and rooted before the host populates them; outgoing graphs are rooted before adapters suspend or transfer ownership. A return-area address alone does not imply ownership of referenced allocations.

`PendingExportResult` manages the lifecycle of both the returned source graph and canonical scratch storage. The export adapter transfers ownership to the callback, ensuring raw ABI values never outlive their owner. Storage is released only after `task.return` consumes the result, or after all operations acknowledge cancellation. Subsequent invocations verify that ownership slots are empty before dispatching new calls.

Regression probes force collection and poison reclaimed storage at worker and callback handoffs. They exercise freshly constructed and imported graphs, text and bytes, nested aggregate results, repeated calls under a memory limit, and cancellation followed by successful reuse.

### Scopes
Module-level bindings and cached process context persist for the lifetime of the component instance. Per-call temporary objects are reclaimed by the canonical post-return hook after results have been transferred to the caller.

Pure Rust guest helpers (such as JSON and date codecs) validate buffer boundaries before borrowing guest memory. Their minimal unsafe boundaries are isolated from pure logic and introduce no auxiliary heap allocators.

## Concurrency

Asynchronous TypeScript functions execute eagerly until they reach an initial suspension point.

### Execution
Completed tasks store their settlement state and outcome value for repeated observation.

Continuations are scheduled directly through native WASI 0.3 wakeups. The runtime does not maintain a secondary event loop or microtask queue.
Awaits on already-settled promises advance inline when uncontended, yielding to the host only when competing work can make progress.
This avoids thousands of redundant host context switches for already-resolved promises, which are
prone to escalate into costly OS-level event polling in common runtimes.

### Settlement
Guest task records maintain explicit settlement states, observer lists, and outcome values in the guest heap. Combinator operations (`all`, `allSettled`, `race`) register as observers on operand tasks and propagate settlements deterministically when dependencies resolve.

Non-winning branches in a race continue running as background tasks. The runtime verifies that all spawned tasks settle or acknowledge cancellation before the top-level export returns; attempting to return while unresolved background work remains results in a runtime trap.

### Streams
Readable stream readers decouple public read promises from underlying native transfer completion:
- Releasing a reader lock rejects pending read promises while preserving native buffer state. Subsequent readers receive any previously buffered chunks.
- Explicit cancellation retains the native transfer owner until host acknowledgement is received.
- Request and response body consumption methods (`text()`, `json()`, `bytes()`, `arrayBuffer()`) pull data incrementally through this same streaming pipeline.
- `for await` loops over byte streams lower into locked reader calls with structured cleanup. Early loop termination explicitly cancels the stream and releases the lock, while loop exceptions take precedence over cancellation errors.

### Host Resource Integration
Host resources—including open file descriptors, network connections, and timers—are tracked via host resource tables. The guest runtime maintains valid handles across asynchronous suspensions and frees them when operations conclude or abort. During abnormal termination or unhandled traps, host store disposal cleans up all active resource tables without invoking guest `finally` dispatch.

## Linked Pure Rust Helpers

Complex algorithmic operations—including JSON serialization, date and time calculations, text transformations, and HTTP header decoding—are implemented as pure Rust helper libraries.

### Bytecode Embedding
Helper libraries are compiled to standalone `wasm32-unknown-unknown` binaries without allocators. The resulting bytecode artifacts are embedded directly into the compiler binary during build time, removing any runtime dependency on Rust or LLVM.

### Capability Pruning
When compiling a user component, the compiler scans AST capability requirements to identify only the helper functions actually referenced by the program. Unreferenced helper functions and dead data sections are pruned completely.

### Static Relocation and Linking
During SSA code generation, the compiler relocates helper functions, signatures, and element tables directly into the target WebAssembly module. Instructions are re-encoded to target local function indices, allowing pure helper routines to execute with zero call overhead.

### Unified Memory Placement
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
2. **Host Integration**: Controlled Wasmtime hosts verify Canonical ABI marshalling, stackful coroutine suspension, cancellation signal propagation, and deterministic resource cleanup across serial invocations.
