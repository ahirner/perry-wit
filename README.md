# perry-wit

Perry-WIT compiles static TypeScript through Perry HIR and WAFFLE SSA into
WebAssembly components targeting WASI 0.3. The production CLI and Rust API use
this pipeline. There is no JavaScript interpreter, legacy emitter, or bundled P2
runtime. See [ARCHITECTURE.md](ARCHITECTURE.md) for implementation boundaries, [PERFORMANCE.md](PERFORMANCE.md) for cutover measurements, and
[TODOs.md](TODOs.md) for the deferred compatibility register.

## Build and verify

```sh
nix develop
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
nix flake check
```

The pinned shell supplies Rust, Node, TypeScript, Wasmtime, and wasm-tools.
`WASI_WIT_PATH` and `WASI_P3_WIT_PATH` select the official WASI 0.3 packages;
`WASI_P2_WIT_PATH` is retained for versioned WIT and host coexistence tests.
Project-local WIT dependencies take precedence over ambient packages of the same
version. Compilation builds and embeds allocation-free text, search, JSON, and
time helpers. No separate guest runtime build or runtime override is required.

`nix build .#perry-wit` packages the compiler. `.#example-merge-docs`,
`.#example-merge-task`, and `.#template-component` build independent examples.
`./scripts/test_e2e.sh` runs the production boundary and P3 HTTP integration tests;
HTTP fixtures own temporary ports and their server lifetimes.

## Compile a component

```sh
cargo run -- examples/merge_task.ts --wit wit --world task-runner -o dist/task.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y \
  --invoke 'run-task("hello")' dist/task.wasm
```

The default world is `command`, which includes `wasi:cli/command@0.3.0`. Its
implementation explicitly exports `runRun(): {ok:true} | {ok:false}` (or a
Promise of that result). Executable module initialization and top-level await are
unsupported; put work inside exports. `examples/merge_docs.ts` demonstrates an
async command making two bounded GET requests to an existing local server.

```sh
cargo run -- examples/merge_docs.ts --world command -o dist/documents.wasm
wasmtime run -C cache=n -S p3=y -S http=y -S inherit-network=y \
  -W component-model-async=y -W component-model-more-async-builtins=y \
  -W component-model-async-stackful=y -W component-model-threading=y \
  dist/documents.wasm
```

Use the pinned Wasmtime and enable those async features when the component needs
them. Existing P2 components can run in the same host; compiled capability calls
use the P3 interfaces. Rebuild old Perry-WIT artifacts and regenerate their SDK
contracts when migrating. Old `--runtime` and raw-emitter APIs are removed.

`--core-only` (also implied by a `.core.wasm` output name) emits core Wasm for
embedding. Raw core callers must implement the declared canonical imports and
call the matching `cabi_post_<export>` after consuming results. Prefer components
for automatic canonical post-return handling.

## Authoring and SDK

Initialize [the template](template/README.md) with the compiler's flake reference,
or enter `nix develop .#sdk` for the component SDK. The template's `nix build`
uses `lib.<system>.buildComponent` with an explicit entry, WIT directory, and world.

```sh
perry-wit gen-types --wit wit --world task --entry src/index.ts
tsc --noEmit
perry-wit src/index.ts --wit wit --world task -o dist/task.wasm
```

Generation writes world and import declarations, P3 capability declarations, and
an implementation check in `.perry/types/`. It creates a missing `tsconfig.json`;
existing configurations must include the generated implementation check.
Interface exports use prefixed implementation names, such as `apiRunTask` for
`api`'s `run-task`. `ComponentImplementation` lists the exact names.

The SDK checks WIT signatures and read-only context. TypeScript's standard library
is broader than the compiler subset: passing `tsc` alone does not imply an API is
supported. The compiler diagnoses excluded operations, including aliases/casts of
read-only context, runtime `delete`, dynamic coercion, and Promise combinators.

## Supported contracts

| Area | Production contract |
| --- | --- |
| WIT | Scalars, strings, records, tuples, enums, variants, results, nullable options, dense typed lists, and up to 32 flags. Sync and directly awaited async imports/exports. Lossless `u64` transport via `bigint`, without arithmetic/coercion. Signed 64-bit values, nested options, and guest resource APIs are unsupported. |
| Records and arrays | Declared fields, typed dictionaries, finite unions, dense homogeneous arrays, checked indices and bounds. No runtime deletion, sparse-array compatibility, or unconstrained `any` coercion. |
| Text | Valid UTF-8 storage; lengths, indexing, slicing, iteration, search, and ordering use Unicode scalars. Surrogate halves and UTF-16 code-unit APIs are rejected. Literal regex supports the documented bounded subset in [the text contract](src/waffle_backend/text_contract.rs). |
| JSON | Strict UTF-8 parsing and compact serialization of primitives, records, value trees, typed arrays, and Date values. Syntax/surrogate/depth-or-cycle errors are numeric `1`/`2`/`3`; unsupported values use `12`. An undefined root produces undefined. No revivers, replacers, indentation, or custom prototype reflection. |
| Context | Cached read-only `process.env`, `process.argv`, and `process.cwd()`. Missing environment keys are undefined. Host arguments have no synthetic Node prefixes; absent cwd becomes `/`. Dictionary enumeration uses insertion order, including numeric-looking keys. |
| Date | `Date.now()`, `new Date(epochMs)`, `.getTime()`, `.toISOString()`. Invalid Date preserves NaN and ISO formatting throws `1`. Constructors from strings, setters, calendar getters, and local-timezone behavior are deferred. |
| Temporal | The existing immutable `Instant` parsing/epoch/formatting and `PlainDateTime` ISO fields/day-offset subset. No timezone database, other durations, rounding, or options. Strict UTC interchange and day shifts have independent fixtures. |
| Clocks and random | Millisecond clocks, immediately awaited `perry:clocks.waitFor`, `Math.random`, v4 UUIDs, and `crypto.getRandomValues(Uint8Array)`. Random fill preserves view identity and checks the 65,536-byte quota before effects. |
| Bytes | `Uint8Array` allocation, literals, copy, indexing, subarray, slice, and view metadata. `TextDecoder` supports strict incremental UTF-8 and BOM handling; replacement decoding and other encodings are unsupported. |
| Filesystem | `fs`/`node:fs` confined preopen access: read/overwrite files, metadata, existence, mkdir/unlink/rmdir, directory names. Reads materialize input; writes accept text or visible byte ranges. Options and flags are validated before I/O. See [declarations](types/p3.d.ts) for overloads and numeric errors. |
| Output | Single-string console log/error/warn, plus immediately awaited `perry:stdio` byte writes. Both stream transfer and separate completion must finish. Multiple arguments and implicit formatting/coercion are unsupported. |
| HTTP client | Immediately awaited `perry:http.get(scheme, authority, path, headers, maxResponseBytes)`. Explicit limit; status, duplicate header bytes, and body bytes preserved. Native resources close and completion is checked before return. General Fetch is deferred. |
| HTTP handler | Rust `waffle_backend::compile_http_handler` exports `wasi:http/handler@0.3.0`. Typed `handle(Request): Response` may be async; explicit request/response caps are mandatory. Records are declared in `perry:http-handler/types`. |

The compiler's [capability catalog](catalog/capabilities.json) links each supported
contract to its tests. The [consumer matrix](TODOs.md#r9--production-cutover-and-retirement)
uses small independent fixtures. Runner is read-only reference material and
an optional external smoke test; its source and WIT are never copied into this repository.

## Async ownership and limits

Resolved WIT exports and HTTP handlers use direct awaits. The lower-level Rust
`compile_typescript_waffle` API additionally supports stored typed tasks and
multiple observers. Promise constructors, `then`/`catch`/`finally` methods,
`Promise.all`, `Promise.race`, callback timers, and detached tasks are deferred
under D5. Supported async execution is eager up to suspension, with single
settlement and retained outcomes for repeated awaits.

WIT exports that can reach a suspending operation must be declared `async func`,
including synchronous-looking filesystem and console calls. The compiler checks
reachable helpers and rejects synchronous WIT exports with those effects.
The TypeScript implementation itself may use a synchronous filesystem spelling.

Public calls are serial. Guest roots retain values across suspension and
collection; canonical post-return releases invocation storage after results are
copied. Cached context survives serial calls. Traps, abandoned pending work, and
interrupted calls require store disposal. Disposal releases native operations;
it does not run guest `finally`. Recoverable source errors still run cleanup.

HTTP client errors include `8` for body overflow, `12` for invalid metadata or
limits, `100 +` the WASI HTTP discriminant, and `200 +` the header discriminant.
Non-2xx status remains a response. Handlers publish responses before body transfer
and retain storage through consumer completion. Hosts must drive the P3 event
loop through completion. Request/response overflow returns the corresponding
body-size error; invalid response metadata or request producer failure returns
`internal-error`. Response status is 200–599; 204/205/304 requires an empty body.
Source streams, response trailers, and stored HTTP tasks are deferred under D6.
EOF alone never implies successful capability completion. Partial writes remain
visible on failure; the compiler does not promise rollback.
