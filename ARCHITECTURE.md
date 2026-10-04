# Perry-WIT architecture

The production path is TypeScript → Perry HIR → WAFFLE SSA → core Wasm →
Component Model encoding. `src/compiler/mod.rs` supplies the public file/source
API and `src/main.rs` the CLI. Both use the same WAFFLE implementation.

## Compilation boundaries

1. `src/waffle_backend/source/modules.rs` resolves static local ESM imports and
   named re-exports. `source.rs` resolves SWC binding identities and normalizes
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
   WAT helpers implement allocation, UTF-8, values, and transfer operations.
   Shared typed WAFFLE builders implement task scheduling, combinators, scalar
   capabilities, and Date storage. Rust helpers supply text/search, JSON, and time codecs.
   Relocatable helper code is linked to the guest's single managed memory; it
   does not bring a second allocator or a dynamic JavaScript dispatcher.
5. `wit.rs` emits canonical adapters for resolved worlds and uses
   `wit-component::ComponentEncoder`. `wit/native.rs` connects selected P3
   capabilities to canonical imports. Bounded incoming HTTP resolves the official
   handler world and uses the same encoder. The lower-level generated-world API
   still uses `component.rs`. The standard
   component representation is validated and stripped for CLI output.

The compiler dependency graph is checked for LLVM/inkwell dependencies. The
compiler backend is pure Rust WAFFLE. Rust's pinned build toolchain still compiles
the allocation-free Wasm helpers; “LLVM-free” describes the compiler's dependency
and link graph, not the implementation of rustc itself.

## Runtime implementation inventory

The backend contains 35 WAT files, totaling 2,235 lines. Each has an active
`include_str!` caller. The table identifies their responsibilities and regression
coverage; these counts exclude WAT assembled inside Rust functions.

| WAT directory | Lines | Caller and responsibility | Validation |
| --- | ---: | --- | --- |
| `allocation` | 251 | `allocation.rs`: canonical realloc, root frames, graph tracing, reclamation | `waffle_string_test`, `waffle_wit_native_test` |
| `bytes` | 64 | `bytes.rs`: byte-view allocation, copying, bounds, and wrapping conversion | `waffle_bytes_test` |
| `context` | 65 | `context.rs`: cached environment, arguments, and cwd with retained guest roots | `waffle_platform/context` |
| `decoder` | 118 | `decoder.rs`: incremental UTF-8 state and BOM handling | `waffle_decoder_test` |
| `filesystem` | 484 | `filesystem.rs`: path/option handling, descriptor operations, buffered reads, metadata, directory transfers; `interfaces.wat` supplies lower-level framing | `p3_filesystem_test` |
| `http` | 345 | `http.rs` and `http/handler.rs`: resource ownership, buffered bodies, completion futures, source records; `interfaces.wat` and `client.wat` supply lower-level framing | `http/tests`, `waffle_http_handler_test`, `waffle_wit_native_test` |
| `json` | 186 | `json.rs`: marshal guest value trees into the Rust codec and build parsed values | `json_helper_test`, `waffle_json_test` |
| `objects` | 97 | `objects.rs`: record/dictionary storage and enumeration | `p3_filesystem/source_options`, `waffle_wit_test` |
| `promises` | 102 | `promises/component.rs`: task runtime for the lower-level generated-world API | `p3_promise_test` |
| `random` | 51 | `random.rs`: bounded random byte transfers and UUID formatting | `waffle_platform_test` |
| `regex` | 44 | `regex.rs`: guest descriptor and scratch storage around the Rust search helper | `waffle_string_test` |
| `streams` | 119 | `streams.rs` and `streams/output.rs`: buffered input, partial writes, output completion | `waffle_stream_test`, `waffle_output_test` |
| `strings` | 38 | `decoder.rs`, `filesystem.rs`, `structured.rs`: strict UTF-8 validation | `waffle_decoder_test`, `p3_filesystem_test` |
| `structured` | 76 | `structured.rs`: Stats and string-list boundary storage | `p3_filesystem/source_structured` |
| `values` | 195 | `values.rs`: tagged values, dense arrays, and checked access | `waffle_wit_test`, `waffle_json_test` |

Resolved-world scheduling and combinators use typed WAFFLE builders in
`promises/native/`. The generated-world framer in `component.rs` and its scalar,
context, filesystem, HTTP, and output adapters also assemble component WAT in
Rust. Those adapters are separate from the official-WIT encoder used by the
CLI and resolved-world API.

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

Resolved worlds support concurrent owned tasks over native P3 threads. Task
records preserve identity, settlement, rejection, and repeated observation. A FIFO
reaction queue orders source continuations; eager guest calls hand control back to
their caller at the first suspension. Native operations can remain pending concurrently.
`all`, `allSettled`, and `race` register operand observers once and share the
settlement/result storage. Race losers retain their owners until completion;
unresolved ordinary work at the call boundary traps. Cooperative cancellation
and source Web Streams are unsupported. Public calls are serial.
Traps and host interruption require store disposal; guest finally blocks do not
execute after disposal.

## Packaging and verification

`flake.nix` pins the compiler toolchain, P3 WIT, host CLI, SDK, and independent
example components. The development shell and component tests use the pinned
WASI 0.3 interfaces.
The SDK shell and Nix component builder share WIT, world, and entry configuration.
An exact compiler-flake revision supplies dependency pins to consumers. Disposable
SDK output lives in ignored `.perry` files; shell entry preserves authored configuration.
Generated SDK declarations describe WIT signatures and P3 capabilities; the
compiler remains the authority for the supported static TypeScript subset.

Tests cover Node comparisons within the matching subset, explicit Unicode
contract differences, canonical ABI shapes, precise capability imports, native
suspension, errors/cancellation, resource release, and repeated calls under
memory caps. Independent source/WIT fixtures encode abstract consumer needs.
Runner source and WIT stay external. The capability catalog and TODO consumer
matrix link contracts to evidence; excluded compatibility cases remain deferred
rather than advertised as supported.
