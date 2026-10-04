# perry-wit

Perry-WIT compiles static TypeScript into WebAssembly components targeting WASI 0.3.
WIT defines component imports and exports. The compiler uses Perry HIR and WAFFLE
SSA, with a Nix SDK for authoring, type checking, and building components.

[Architecture](ARCHITECTURE.md) describes the compiler and runtime boundaries;
[performance](PERFORMANCE.md) records size, memory, and throughput measurements.

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
time helpers.

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

Select `--world command` to use `wasi:cli/command@0.3.0`. Its
implementation explicitly exports `runRun(): {ok:true} | {ok:false}` (or a
Promise of that result). Executable module initialization and top-level await are
unsupported; put work inside exports. `examples/merge_docs.ts` demonstrates an
async command making two bounded GET requests to an existing local server.
Packages with multiple worlds require an explicit world selection in both the
compiler and SDK generator.

```sh
cargo run -- examples/merge_docs.ts --world command -o dist/documents.wasm
wasmtime run -C cache=n -S p3=y -S http=y -S inherit-network=y \
  -W component-model-async=y -W component-model-more-async-builtins=y \
  -W component-model-async-stackful=y -W component-model-threading=y \
  dist/documents.wasm
```

Use the pinned Wasmtime and enable those async features when the component needs
them. Compiled platform capabilities use the standard P3 interfaces.

`--core-only` (also implied by a `.core.wasm` output name) emits core Wasm for
embedding. Raw core callers must implement the declared canonical imports and
call the matching `cabi_post_<export>` after consuming results. Prefer components
for automatic canonical post-return handling.

## Authoring and SDK

Initialize [the template](template/README.md) with the compiler's flake reference.
Its `nix build` and `nix develop` use `lib.<system>.buildComponent` and `mkSdkShell`
with the same entry, WIT directory, and world. Inside this compiler checkout,
`nix develop .#sdk` generates the SDK for `examples/merge_task.ts`.
Pin the compiler flake to an exact revision to inherit its toolchain and dependency
pins. Projects using only those pinned inputs need no separate consumer lockfile.
Generated `.perry` files are ignored. Use `nix develop --no-write-lock-file` and
`nix build --no-write-lock-file`.

```sh
perry-wit gen-types --wit wit --world task --entry src/index.ts
tsc --noEmit -p .perry/types
perry-wit src/index.ts --wit wit --world task -o dist/task.wasm
```

Generation writes world and import declarations, P3 capability declarations, and
an implementation check in `.perry/types/`. The generated check configuration
extends an existing project configuration and always checks the selected entry.
Explicit generation creates a missing `tsconfig.json`; `--no-tsconfig`, used by
the SDK shell, leaves authored files alone.
Interface exports use prefixed implementation names, such as `apiRunTask` for
`api`'s `run-task`. `ComponentImplementation` lists the exact names.

The SDK checks WIT signatures and read-only context. TypeScript's standard library
is broader than the compiler subset: passing `tsc` alone does not imply an API is
supported. The compiler diagnoses excluded operations, including aliases/casts of
read-only context, runtime `delete`, and dynamic coercion. Static local ESM
imports, named export aliases, and named re-exports share the same source with Node.

## Supported contracts

| Area | Production contract |
| --- | --- |
| WIT | Scalars, strings, records, tuples, enums, variants, results, nullable options, dense typed lists, and up to 32 flags. Sync exports and owned async tasks. Lossless `u64` transport via `bigint`, without arithmetic/coercion. Signed 64-bit values, nested options, and guest resource APIs are unsupported. |
| Records and arrays | Declared fields, typed dictionaries, finite unions, dense homogeneous arrays, checked indices and bounds. No runtime deletion, sparse-array compatibility, or unconstrained `any` coercion. |
| Text | Valid UTF-8 storage; lengths, indexing, slicing, iteration, search, and ordering use Unicode scalars. Surrogate halves and UTF-16 code-unit APIs are rejected. Literal regex supports the documented bounded subset in [the text contract](src/waffle_backend/text_contract.rs). |
| JSON | Strict UTF-8 parsing and compact serialization of primitives, records, value trees, dense typed lists, and Date values. Syntax/surrogate/depth-or-cycle errors are numeric `1`/`2`/`3`; unsupported values use `12`. An undefined root produces undefined. No revivers, replacers, indentation, or custom prototype reflection. |
| Context | Cached read-only `process.env`, `process.argv`, and `process.cwd()`. Missing environment keys are undefined. Host arguments have no synthetic Node prefixes; absent cwd becomes `/`. Dictionary enumeration uses insertion order, including numeric-looking keys. |
| Date | `Date.now()`, `new Date(epochMs)`, `.getTime()`, `.toISOString()`. Invalid Date preserves NaN and ISO formatting throws `1`. Constructors from strings, setters, calendar getters, and local-timezone behavior are deferred. |
| Temporal | Immutable `Instant` parsing/epoch/formatting and `PlainDateTime` ISO fields/day-offset subset. No timezone database, other durations, rounding, or options. Strict UTC interchange and day shifts have independent fixtures. |
| Clocks and random | Millisecond clocks, promise-based `node:timers/promises.setTimeout`, `Math.random`, v4 UUIDs, and `crypto.getRandomValues(Uint8Array)`. Timers clamp and truncate delays using Node's rules. Random fill preserves view identity and checks the 65,536-byte quota before effects. |
| Bytes | `Uint8Array` allocation, literals, copy, indexing, subarray, slice, and view metadata. `TextDecoder` supports strict incremental UTF-8 and BOM handling; replacement decoding and other encodings are unsupported. |
| Filesystem | `node:fs/promises` readFile/writeFile/stat/mkdir/unlink/rmdir/readdir, with synchronous spellings under `fs`/`node:fs` and existsSync. Paths are confined to preopens. Reads materialize input; writes accept text or visible byte ranges. Options and flags are validated before I/O. See [declarations](types/p3.d.ts) for overloads and numeric errors. |
| Output | Single-string console log/error/warn, plus owned `perry:stdio` byte writes. Both stream transfer and separate completion must finish. Multiple arguments and implicit formatting/coercion are unsupported. |
| HTTP client | Concurrent owned `perry:http.get(scheme, authority, path, headers, maxResponseBytes)` tasks. Explicit limit; status, duplicate header bytes, and body bytes preserved. Native resources close and completion is checked before settlement. Fetch and source Web Streams are not yet implemented. |
| HTTP handler | Rust `waffle_backend::compile_http_handler` exports `wasi:http/handler@0.3.0`. Typed `handle(Request): Response` may be async; explicit request/response caps are mandatory. Records are declared in `perry:http-handler/types`. |

The compiler's [capability catalog](catalog/capabilities.json) records supported
type shapes, WASI mappings, limitations, and tests.

## Async ownership and limits

Resolved WIT components support stored typed tasks, multiple observers, and
`Promise.all`, `Promise.allSettled`, and `Promise.race` over dense typed arrays
and tuples. Async execution is eager up to suspension, with one settlement and
retained outcomes for repeated awaits. Combinators register each operand once
and preserve input order where required. `race` leaves losing operations running;
callers must await their completion before returning. Cooperative cancellation
is not yet exposed. Promise constructors, callback reactions, callback timers,
and detached tasks remain unsupported.

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
Source Web Streams, response trailers, and cooperative cancellation are unsupported.
EOF alone never implies successful capability completion. Partial writes remain
visible on failure; the compiler does not promise rollback.
