# Architecture

Perry-WIT compiles a static subset of TypeScript to WASI 0.3 components. [TODOs.md](TODOs.md)
records the completion checklist and accepted deviations.

## Crates

| Crate | Responsibility |
|---|---|
| `perry-wit` | CLI, compilation API, WIT binding, SDK generation, linking and component encoding |
| `perry-lower` | Static source-module loading and Perry HIR → typed MIR |
| `perry-mir` | Types, operations, verifiers, effects, escape analysis and optimization |
| `perry-codegen` | Value representations, stackless async splitting and canonical ABI glue |
| `pliron-wasm` | Wasm dialect, CFG structuring and core-module emission |
| `perry-runtime` | Allocation, collection, roots, tasks, promises, strings and arrays |
| `perry-std` | Built-ins over native values: bytes, JSON, regex tables and decoding |
| `perry-fs` | Promise-only filesystem operations over WASI |
| `perry-web` | Fetch, URL, headers, request/response values and byte streams |
| `perry-abi` | Runtime services and layouts shared by guest libraries |
| computation helpers | Parsing, Unicode case conversion, search, time and number formatting |
| `conformance` | Directed contracts, generated programs, shrinking and replay against Node |

## Compilation

```text
Static TypeScript modules → Perry HIR → typed MIR
    → SSA promotion, bounded inlining, scalar replacement, DCE, CFG simplification
    → escape/effect summaries and lifetime assignment
    → Wasm dialect, canonical conversion, async state machines
    → core Wasm, linked runtime services, component encoding
```

Locals initially use slots; `mem2reg` builds SSA joins and loop-carried values. Record shapes
contain sorted field names and their static types, while literal expressions evaluate in source
order. Named records can be recursive. Unsupported constructs receive compiler diagnostics.

The emitter makes resumed CFGs reducible, structures control flow using dominators, folds single-use
values into stack expressions and preserves effect order. The linker resolves runtime imports,
relocates reachable helper functions and reserves their combined stack requirements. Every emitted
core module and component is validated with thread features, stack switching, stackful component
async and additional async builtins disabled.

## Values

| Value | Representation |
|---|---|
| number | IEEE-754 `f64` |
| boolean | `i32` |
| record | Address of a shape identifier and statically typed eight-byte field slots |
| dense array | Address of length, capacity and eight-byte element storage |
| string | Immutable view over UTF-8 bytes, with cached scalar count and owner |
| Uint8Array | Mutable byte view retaining its storage owner |
| bigint | Boxed exact sign/magnitude for WIT s64/u64 transport |
| Date | Mutable boxed epoch-millisecond value |
| Temporal | Immutable 16-byte codec payload: epoch nanoseconds for Instant, epoch day plus nanoseconds within the day for plain values |
| JSON | Immutable parsed graph, distinct from native aggregate storage |
| optional | The underlying representation plus absence sentinels admitted by its type |

References encode `undefined` as zero and `null` as one, retaining a single-word representation.
Numbers reserve signalling-NaN payloads and booleans use spare integer values. Stable absence
sentinels let closed records and arrays widen admitted absence states without copying or losing aliases.

Strings use Unicode scalar indexing. ASCII character strings are static; a cached scalar cursor
accelerates sequential non-ASCII indexing. Slices share immutable storage. Append chains reuse
spare buffer capacity while the left operand is the latest view; retained prefixes remain unchanged.

Dates preserve identity and alias-visible mutation. Literal regular expressions compile into
bounded DFA tables; search/test allocate no guest storage. Incremental TextDecoder retains at most
three pending bytes, with explicit fatal-error and BOM state.

## JSON

The parser produces immutable JSON nodes. Required scalar conversions check node kinds. Conversion
into a declared closed record or dense array creates independent native aggregates; one generated
helper per schema handles recursive types. Absence checks distinguish missing members and null.

Serialization of native aggregates is generated per type. JSON graph traversal and parsing use
bounded iterative helper stacks. Object spreads copy shallow JSON member nodes and preserve the
supported ECMAScript property order. Revivers, replacers and indentation remain compile-time errors.

## Memory

`talc` manages linear memory after static data and helper stacks. The compiler assigns these lifetimes:

| Tier | Storage policy |
|---|---|
| scalar | Eliminate closed aggregates whose fields can stay in SSA |
| frame | Reuse bounded activation storage for proven nonescaping scalar records |
| task | Regions for statically bounded task storage and recycled scheduler nodes |
| instance | Precise non-moving collection for persistent or unproven allocation lifetimes |
| buffer | Explicit owners for canonical and host-transfer storage |

Collectible objects use size-class pages, direct page lookup, side mark bits, exact pointer shapes,
a tracing worklist and lazy sweeping. Ordinary collection is due at
`max(256 KiB, live bytes after collection)` allocated volume; memory pressure can request an earlier
collection at a compiled safe point. Helpers allocate without collecting internally. The remaining
heap-growth and transfer stress acceptance work is tracked in the checklist.

Roots comprise live compiled values, persistent module cells, suspended frames and active operations.
Completed operations detach their edges, dead frame slots clear, and recycled scheduler entries clear
their old references. Views retain their owner. Pins remain until host completion or cancellation
acknowledgement; one task's cleanup preserves another task's storage.

Stream leases require a liveness proof: a previous read/write result and every derived value must be
dead before reuse. Otherwise the operation uses collectible storage. Writer close waits for queued
writes and propagates failures. Async iteration releases the reader and cancels on early exit/errors.

BYOB readers transfer a shared byte-buffer owner without copying its payload. Views retain an owner,
offset and length; a detached owner invalidates every alias and releases its payload edge. Queued
reads retain only active requests. A host copy pins its reader until completion or cancellation
acknowledgement. Canonical transfers retain the payload independently of later buffer detachment.

`--explain-tiers` reports allocation decisions. Raw test modules can expose allocation, collection,
live-byte, reserved-byte, transfer-copy, pin and root counters, plus forced collection and poisoning.

## Boundaries

The selected WIT world supplies function names, types and async effects. SDK type imports resolve to
these static definitions. Extra source exports stay internal. Static local modules share one typed
module state; synchronous initialization runs once, with failure/reentrancy checks.

Finite variants occupy sixteen bytes: a static tag string, one traced owner and one payload slot.
Tag guards select statically checked payload types; access checks remain valid across alias changes.

Canonical conversion supports scalars, records, lists, tuples, options, enums, finite variants
and results. WIT s64/u64 use bigint for exact transport. Tagged results use `{ ok: true, value }` or
`{ ok: false, error }`, omitting absent payloads. An explicit tagged return takes precedence over a
completion-style payload. Compatible throws become `err`; unsupported mappings receive diagnostics.

Outbound `WitInput<T>` recursively permits strings for byte lists. Lowering uses the string's UTF-8
pointer and byte length. Canonical inbound strings and byte lists adopt transferable payload storage;
byte lists remain mutable without aliasing immutable strings. Output owners survive lifting and
post-return or async acknowledgement.

Named WIT imports use the same conversion routines. Async imports lower to a small MIR wrapper that
starts a host call, awaits its transfer and lifts the result. The transfer retains argument owners,
temporary canonical buffers and result areas. Cancellation acknowledges the host before discarding
returned canonical buffers and clearing transfer owners.

Resource descriptors share ownership state across aliases. Canonical argument preparation is
transactional: a duplicate move rolls back before invoking the host. Active transfers root borrowed
owners until completion or cancellation acknowledgement. The collector's resource registry is weak;
it queues unreachable owned handles after marking. Generated adapters drain that queue at legal
host-call boundaries, before return and outside collector borrows. Saved inbound borrows expire at
task completion. Imported resources can pass through exported interfaces using WIT type aliases.
Declaring one resource interface as both imported and exported is diagnosed because its two resource
identities need separate source types. Asynchronous module initialization remains incomplete.

## Async

Async functions split into a ramp and a resumable step. Calls start eagerly. A settled await continues
inline; a pending await registers the frame as a waiter. The runtime drives ready frames after P3
completion events. Promise combinators use the same task queue.

Each export has its own task, waitable set, active host operations and scheduler storage. The callback
ABI stores that task in `context.set 0`; completion resumes it through `context.get 0`. Once the result
and required output transfers finish, `task.return` delivers the value and cleanup releases the task.
Exports and host calls can overlap without requiring component threading.

Unresolved detached timer/import/filesystem/fetch/read work is cancelled at the task boundary and
reported as a failure. Host cancellation callbacks detach waitables and acknowledge native transfers
before releasing task storage. A host-cancelled activation is discarded; source `finally` guarantees
apply to source control flow. Promise timers accept `AbortController.signal`; active watches reuse
scheduler entries and detach when timers settle. Source cancellation is limited to the current export
task; attempts to cancel another task through a shared signal throw before cancelling operations.
Source cancellation options for other builtins are deferred unless required by an application.

## Exceptions

Catchable failures carry an exception status and optional exception object through explicit CFG edges.
Callers propagate failures to their nearest handler. `finally` runs along normal completion, throws,
returns and loop exits. A rejected promise retains its exception for repeated observation.

An escaping exception becomes the declared compatible WIT error, traps an export without an error
mapping, or fails a CLI command after reporting to stderr. Resource exhaustion remains a Wasm trap.

## Verification

Unit tests check dialect invariants, emission, runtime ownership and pure helpers. Integration tests
exercise compiled core modules, actual WIT worlds, SDK checking, in-process HTTP, bounded memory,
stream reuse and suspended calls. Node supplies the oracle for declared shared semantics; astral
scalar string behavior has explicit Perry expectations.

Generated programs use bounded static types, valid metamorphic forms and replayable seeds. Compiler
rejection is a failure. Scaling checks compare guest fuel at n and 4n; component budgets use frozen
source baselines. Final consumer acceptance is recorded separately after its approved run.
