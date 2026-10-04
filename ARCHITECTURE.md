# Perry-WIT architecture

Perry-WIT compiles static TypeScript to WASI 0.3 components through Perry HIR and
WAFFLE SSA. The CLI, public Rust API, Nix builds, HTTP handler compiler, and
component tests use the same resolved-WIT component encoder.

## Architectural principles

1. **Ahead-of-time execution.** Source operations become Wasm instructions.
   Primitive locals travel through SSA values and block parameters; aggregates
   and retained values live in managed guest memory.
2. **Static source contracts.** Binding-aware checks reject unsupported dynamic
   forms while the AST still contains the information needed for useful
   diagnostics. Runtime checks handle data-dependent bounds and finite variants.
3. **Direct HIR-to-SSA lowering.** Branches, loops, exceptions, calls, and awaits
   share the same lowering and validation machinery. Typed capability plans
   connect source validation to implementation signatures and canonical imports.
4. **Native Component Model async.** Host-managed suspension and native transfer
   handles carry pending operations. Guest task records and a reaction queue
   supply source Promise identity, observation, and combinator ordering.
5. **Authoritative WIT.** Pinned official interfaces define built-in platform
   capabilities. Application WIT defines application contracts. SDK and compiler
   share export naming; internal helper names do not create host protocols.
6. **One guest heap.** Pure algorithms use small Rust helpers linked to the
   existing memory. Allocation, roots, canonical scratch, and suspended source
   values follow explicit guest ownership.
7. **Reproducible builds and evidence.** Nix pins tools and interfaces. Node
   comparisons establish source behavior; controlled P3 hosts establish ABI,
   suspension, ownership, and resource cleanup.

The compiler's dependency and link graph excludes LLVM and inkwell. WAFFLE is
the Rust compiler backend; the pinned Rust toolchain still uses its own backend
to build the allocation-free Wasm helpers.

## Compilation pipeline

```text
 TypeScript entry + static ESM dependencies       Application + official WIT
                     │                                      │
          Parse and resolve bindings               Resolve packages/world
                     │                                      │
       Validate source forms and normalize capabilities     │
                     │                                      │
                 Perry HIR ───── check signatures/effects ───┘
                     │
          Lower into typed WAFFLE SSA
          + canonical ABI adapters
          + guest runtime and Rust helpers
                     │
       Validate/optimize and emit core Wasm
                     │
           Embed resolved WIT metadata
                     │
       ComponentEncoder → validated P3 component
```

### 1. Parse and resolve source

`source/modules.rs` loads static local ESM dependencies and named re-exports.
SWC binding identities distinguish built-ins from shadowed local names.
`source.rs` normalizes supported capabilities before Perry HIR lowering;
specialized checks enforce read-only context, static types, text, and supported
Date/Temporal forms. Argument and receiver evaluation retain source order.

### 2. Establish contracts and initialization

`resolve.rs` and `wit.rs` validate source functions, WIT shapes, and reachable
effects before component encoding. `component/wit.rs` resolves versioned packages
with local dependency precedence and rejects ambiguous world selection.

`initialization.rs` creates a command adapter only for a world exporting the
standard CLI run interface. It also extracts module evaluation into a guarded
initializer with uninitialized, running, ready, and failed states. Static
dependencies initialize once before the first public call. Retained bindings
persist across calls; accesses before initialization fail. A synchronous WIT
export cannot depend on initialization that may suspend.

### 3. Lower to WAFFLE SSA

`ssa/` constructs blocks for source control flow. Branch joins and loop headers
carry local values as block parameters. Shared exception paths preserve
catch/finally behavior. Source calls, native operations, and Promise observation
use common type and ownership rules. Reference-bearing values occupy root frames
that remain linked across suspension.

Typed builders in `runtime/builder.rs` construct scheduling, combinator, scalar
capability, process-context, transfer, output, and Date helpers. Pure text/search,
JSON, and time algorithms are
compiled from Rust and linked into the same module and memory.

### 4. Emit core Wasm

WAFFLE validates SSA and reducible control flow, optimizes the graph, and recovers
structured Wasm. Core output is available for compiler testing or embedding;
encoding a component requires an explicit resolved world. Canonical scratch and
return layouts come from WIT types, not from duplicated interface definitions.

### 5. Encode the component

`wit/adapter.rs` emits import/export marshalling. `wit/native.rs` connects the
selected P3 capabilities to canonical imports. `component::encode_resolved`
embeds the resolved world and delegates to `wit_component::ComponentEncoder`.
HTTP handlers resolve the official handler world and use this same path.
Component WAT is an inspection artifact, not an intermediate assembly format.

## Source and host conformance

```text
  Ordinary TypeScript ──────────── Node execution
          │                            │
          │       compare supported    │
          │       source behavior      │
          ▼                            │
     Perry compiler                    │
          │                            │
          ▼                            │
     P3 component ────────────── observable results
          │
          ▼
  Controlled Wasmtime host
  • exact imports/exports and canonical types
  • gated concurrency, cancellation, and completion
  • native resource tables and memory limits
```

Node comparisons cover evaluation order, lexical scope, values, errors, and
supported standard APIs. CLI fixtures run as scripts; export fixtures use normal
imports and explicit calls. Application WIT fixtures own their Node bindings.

Host tests separately verify canonical marshalling, capability selection, native
suspension, partial transfers, separate completion failures, and cleanup across
serial calls. Deterministic gates establish overlap without relying on elapsed
time. Cancellation probes exercise the pinned runtime's canonical protocol;
those probes do not imply source-level AbortController support.

The [capability catalog](catalog/capabilities.json) is the support register.
`scripts/check_conformance.mjs` discovers current Cargo test executables, validates
exact catalog test identifiers, and combines their execution outcomes with fresh
Node comparisons. Missing, skipped, or failed evidence prevents completion.
Deviations such as Unicode scalar indexing are explicit contracts. Generated
declarations are type-checking inputs, not evidence of implemented behavior.
Fixtures are independently authored; Runner source and WIT remain external.

## Values and memory

Strings store validated UTF-8 and use Unicode scalar indexing. Source surrogates
and UTF-16 code-unit operations receive diagnostics. Canonical strings, JSON,
and the current strict TextDecoder validate external text; binary payloads remain
bytes.

Records, dictionaries, finite unions, JSON trees, byte views, and Promise outcomes
share a non-moving managed heap. Runtime tags distinguish finite representations
where static type information alone is insufficient. Loop collection traces
linked roots and coalesces dead allocations. Canonical import scratch has its
own root scope, retaining buffers while sibling tasks allocate and collect.
Module bindings and cached context have instance lifetimes. Canonical post-return
releases invocation temporaries after results have been copied.

JSON and time guest adapters validate bounds and overlap before borrowing guest
memory, then write into caller-owned output. Their small unsafe boundary is
separate from the independently tested pure codecs and introduces no allocator.

## Async execution and ownership

Source async functions execute eagerly to their first suspension. Stored tasks
keep one settlement and shared outcome storage for repeated awaits. A FIFO
reaction queue schedules source continuations. `all`, `allSettled`, and `race`
register each operand once; race losers retain ownership and continue executing.
Returning with unresolved ordinary work traps. Public invocations are serialized.

Native transfer loops handle partial reads/writes and backpressure, and check
the associated completion channel as well as EOF. Filesystem descriptors and
HTTP resources close on supported return/error paths. Validation precedes
external effects; already transferred bytes cannot be rolled back. The current
bounded HTTP handler retains its storage through response consumer completion.

Production suspension uses native stackful workers. Test-owned callback adapters
also verify acknowledged native subtask cancellation and worker cleanup before
publishing terminal results. Connecting that cancellation owner to source APIs,
and exposing Web Streams with returned-stream ownership, remain requirements in
[TODOs.md](TODOs.md). Traps and interrupted calls require store disposal; disposal
releases native operations without executing guest `finally` blocks.

## Runtime WAT inventory

The following families still contain production WAT. They implement guest
runtime behavior, not alternative component framers. Their replacement with pure
Rust algorithms or typed WAFFLE builders remains consolidation work.

| Family | Caller and responsibility | Executable coverage |
| --- | --- | --- |
| `allocation` | `allocation.rs`: realloc, tracing, roots, reclamation | `waffle_string_test`, `waffle_wit_native_test` |
| `bytes` | `bytes.rs`: byte-view allocation, copying, checked access | `waffle_bytes_test` |
| `decoder`, `strings/utf8` | `decoder.rs`, `filesystem.rs`, `structured.rs`: UTF-8 and incremental decoding | `waffle_decoder_test`, `p3_filesystem_test` |
| `filesystem` | `filesystem.rs`: paths, options, descriptors, metadata, buffered transfers | `p3_filesystem_test` |
| `http` | `http.rs`, `http/handler.rs`: resources, bodies, completion, source records | `http/tests`, `waffle_http_handler_test`, `waffle_wit_native_test` |
| `json` | `json.rs`: guest value-tree codec adapter | `json_helper_test`, `waffle_json_test` |
| `objects` | `objects.rs`: records, dictionaries, enumeration | `p3_filesystem/source_options`, `waffle_wit_test` |
| `random` | `random.rs`: byte transfers and UUID formatting | `waffle_platform_test` |
| `regex` | `regex.rs`: search helper descriptors and scratch | `waffle_string_test` |
| `streams/runtime.wat` | `streams.rs`: invocation-owned byte input and chunk storage | `waffle_stream_test` |
| `structured` | `structured.rs`: Stats and string-list storage | `p3_filesystem/source_structured` |
| `values` | `values.rs`: tagged values, dense arrays, checked access | `waffle_wit_test`, `waffle_json_test` |

The callers also assemble core runtime fragments in Rust. Moving a WAT body
into a Rust string does not remove that responsibility. Only small, documented,
tested ABI bridges are intended to remain after consolidation.

## Module boundaries

| Module | Responsibility |
| --- | --- |
| `src/main.rs`, `src/compiler/` | CLI and public file/source compilation |
| `src/waffle_backend/mod.rs` | Orchestrate lowering and retain inspectable artifacts |
| `src/waffle_backend/source/`, `resolve.rs` | Static source checks, identities, typed operations |
| `src/waffle_backend/initialization.rs` | Command adaptation and instance module state |
| `src/waffle_backend/ssa/` | Shared values, control flow, exceptions, and source calls |
| `src/waffle_backend/capabilities/` | Pure capability plans and scalar implementations |
| `src/waffle_backend/promises/native/` | Task records, observers, reaction queue, combinators |
| `src/waffle_backend/allocation/`, `allocation.rs` | Guest heap, roots, and canonical scratch scopes |
| `src/waffle_backend/wit/`, `wit.rs` | Canonical layouts, adapters, native capability wiring |
| `src/component/` | Official/application WIT resolution and component encoding |
| `src/sdk/` | Generated SDK contracts and shared implementation names |
| `src/conformance/`, `tests/` | Source comparisons, capability evidence, controlled hosts |
| `crates/` | Pure algorithms and checked guest adapters |

Internal interfaces are narrow `pub(crate)` or parent-module exports. Runtime
builders mutate the Wasm module; capability plans and type/layout decisions are
pure values consumed by those builders.

## Packaging

`flake.nix` pins the toolchain, official P3 WIT, host CLI, and SDK. Component
builders and SDK shells share entry/WIT/world configuration. An exact compiler
flake revision supplies its dependency pins to consumers; generated `.perry`
files are ignored and shell entry preserves authored configuration.

[PERFORMANCE.md](PERFORMANCE.md) records measurement methods and historical
results. Current tests measure production components without maintaining a
second compiler or component encoding path.
