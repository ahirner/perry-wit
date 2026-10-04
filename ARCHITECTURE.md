# Perry-WIT architecture

The production path is TypeScript → Perry HIR → WAFFLE SSA → core Wasm →
Component Model encoding. `src/compiler/mod.rs` supplies the public file/source
API and `src/main.rs` the CLI. Both use the same WAFFLE implementation. The legacy
emitter, guest dispatcher/runtime, static runtime merger, and textual memory
replacement have been removed.

## Compilation boundaries

1. `src/waffle_backend/source.rs` resolves SWC binding identities and normalizes
   supported capabilities before Perry HIR lowering. Separate checks enforce
   read-only context, static value types, supported Date/Temporal forms, and
   evaluation order. Unsupported dynamic forms fail before lowering discards
   information needed for diagnostics.
2. `src/waffle_backend/resolve.rs` and `wit.rs` validate function contracts and
   determine types, capability imports, and canonical adapters. Resolved worlds
   use SDK implementation names and typed WIT imports. Versioned packages are
   resolved by `src/component/wit.rs`, with local dependency precedence.
3. `src/waffle_backend/ssa/` lowers values, branches, loops, exceptions, source
   calls, and supported awaits to WAFFLE blocks. Primitive locals are SSA values;
   reference-bearing values use typed root frames. Shared capability plans drive
   validation, core signatures, and component wiring.
4. WAFFLE validates and optimizes the SSA module and emits structured core Wasm.
   Small WAT helpers still implement live allocation, UTF-8, value, Promise, and
   transfer operations. Rust helpers supply text/search, JSON, and time codecs.
   Relocatable helper code is linked to the guest's single managed memory; it
   does not bring a second allocator or a dynamic JavaScript dispatcher.
5. `wit.rs` emits canonical adapters for resolved worlds and uses
   `wit-component::ComponentEncoder`. `wit/native.rs` connects selected P3
   capabilities to canonical imports. The lower-level generated-world API uses
   `component.rs`; bounded incoming HTTP uses `http/handler`. The standard
   component representation is validated and stripped for CLI output.

The compiler dependency graph is checked for LLVM/inkwell dependencies. The
compiler backend is pure Rust WAFFLE. Rust's pinned build toolchain still compiles
the allocation-free Wasm helpers; “LLVM-free” describes the compiler's dependency
and link graph, not the implementation of rustc itself.

## Values and memory

Strings contain validated UTF-8 and use scalar indexing. The frontend rejects
unpaired source surrogates and code-unit operations. Strict JSON, TextDecoder,
and canonical string boundaries validate external text. Binary payloads remain
bytes. Rust host diagnostic rendering is separate from guest string semantics.

Records, typed dictionaries, finite unions, JSON trees, byte views, and supported
Promise outcomes share managed guest storage. Runtime tags remain where needed
for unions and parsed value trees; a static source contract does not eliminate
all runtime checks. Root frames retain values across calls and suspension. Loop
collection traces reachable allocations and coalesces dead storage. Post-return
reclaims invocation temporaries after the host copies canonical results.

The JSON/time guest adapters borrow validated memory ranges and write to
caller-owned output. They validate bounds and overlap before creating Rust
references. These are the explicitly approved unsafe boundaries, without a
separate runtime allocator. The pure codec logic is tested independently.

## Native async and ownership

P3 capabilities use native Component Model streams, futures, and host-managed
suspension. Shared transfer loops handle partial reads/writes, backpressure, EOF,
and separate producer/consumer completion. Filesystem descriptors and HTTP
resources close on supported return/error paths. Validation precedes external
side effects. A failed write does not roll back bytes already sent.

Resolved worlds support directly awaited async imports/exports. Stored tasks
and observers remain available through the lower-level generated-world API;
its task records preserve identity, settlement, rejection, and observed ordering.
Promise combinators and detached work are explicitly deferred. Public component
calls are serial. Traps and host interruption require store disposal; guest
finally blocks do not execute after disposal.

## Packaging and verification

`flake.nix` pins the compiler toolchain, P3 WIT, host CLI, SDK, and independent
example components. P2 WIT and host bindings remain only for versioned contracts
and P2/P3 coexistence checks. They are not an alternative compiler backend.
Generated SDK declarations describe WIT signatures and P3 capabilities; the
compiler remains the authority for the supported static TypeScript subset.

Tests cover Node comparisons within the matching subset, explicit Unicode
contract differences, canonical ABI shapes, precise capability imports, native
suspension, errors/cancellation, resource release, and repeated calls under
memory caps. Independent source/WIT fixtures encode abstract consumer needs.
Runner source and WIT stay external. The capability catalog and TODO consumer
matrix link contracts to evidence; excluded compatibility cases remain deferred
rather than advertised as supported.
