# perry-wit

Perry-WIT compiles static TypeScript ahead of time into WebAssembly components
targeting WASI 0.3. It lowers TypeScript through Perry HIR and WAFFLE SSA; WIT
defines the component's imports and exports.

Write a command as an ordinary TypeScript script, or implement a component with
named exported functions. Static local ESM imports, aliases, and named re-exports
let both entry styles share code with Node tests.

[ARCHITECTURE.md](ARCHITECTURE.md) explains the compiler, memory, and async model.
The [capability catalog](catalog/capabilities.json) records supported type shapes,
WASI mappings, limitations, and tests. TypeScript declarations describe a broader
API surface than the compiler implements; type checking alone does not establish
compiler support.

## Environment

For compiler development, enter the repository's pinned environment:

```sh
nix develop
```

It supplies Rust, Node.js, TypeScript, Wasmtime, and wasm-tools. `WASI_WIT_PATH`
and `WASI_P3_WIT_PATH` identify the official WASI 0.3 packages. Application WIT
dependencies in `wit/deps` take precedence over ambient packages of the same
version.

For component development, use [the starter template](template/README.md). Its
`nix develop` supplies the packaged compiler and Node tools and generates the
selected world's SDK. In this checkout, `nix develop .#sdk` generates the SDK for
`examples/merge_task.ts`.

## Usage

### Build

```sh
nix build .#perry-wit
nix build .#example-merge-task
nix build .#template-component
```

For a local compiler build inside the development shell:

```sh
cargo build --release
```

### Compile and run a script

A world exporting `wasi:cli/run@0.3.0` turns top-level statements and top-level
`await` into a command. Importing CLI capabilities alone does not create one.
For example, this script runs directly under Node:

```ts
import { setTimeout } from "node:timers/promises";

await Promise.all([setTimeout(2), setTimeout(1)]);
console.log("done");
```

Save it as `example.ts` in this checkout and select its command world:

```sh
node example.ts
cargo run -- example.ts --wit wit --world command -o dist/example.wasm
wasmtime run -C cache=n -S p3=y \
  -W component-model-async=y -W component-model-async-stackful=y \
  -W component-model-more-async-builtins=y -W component-model-threading=y \
  dist/example.wasm
```

Use the pinned Wasmtime with the async features above for suspending components.
Network and filesystem access additionally require the corresponding host grants.
`process.exitCode = 1` sets the command status after execution finishes;
`process.exit(1)` terminates immediately. Uncaught source failures return a failed
WASI command result.

### Compile and call exported functions

WIT is authoritative for names, parameter/result types, and async effects.
Both `s64` and `u64` use TypeScript `bigint`, including nested records, lists,
options, and tuples. Literal record field names remain unchanged when they
match builtins such as `fetch`, `process`, or `console`. An export such as `run-task: func(input: string) -> string` is implemented with:

```ts
export function runTask(input: string): string {
  return "received: " + input;
}
```

```sh
cargo run -- examples/merge_task.ts --wit wit --world task-runner -o dist/task.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y \
  --invoke 'run-task("hello")' dist/task.wasm
```

Components may export multiple functions. Interface members use the SDK's
prefixed names, such as `apiRunTask` for `api`'s `run-task`.
`ComponentImplementation` lists the exact names. Missing implementations,
incompatible signatures, ambiguous worlds, and unsupported export forms receive
compiler diagnostics.

Static dependencies and top-level initialization execute once before the first
public call; module state persists across later calls. A WIT export must be
`async func` if it can suspend, including during module initialization or through
filesystem and console operations. Public invocations are serialized.

`--core-only` (also implied by a `.core.wasm` output name) emits core Wasm for
embedding. Raw core callers provide the declared canonical imports and call the
matching `cabi_post_<export>` after consuming results. Component callers get
canonical result cleanup automatically.

### Test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
./scripts/test_e2e.sh
node scripts/check_conformance.mjs
nix flake check
```

Tests compare supported source behavior with Node and exercise components against
controlled P3 hosts. The conformance check runs fresh Rust and Node tests, validates
the catalog's exact test identifiers, and rejects missing or skipped evidence.
Its report is written to `target/conformance/report.json`. HTTP tests own ephemeral
endpoints. Performance workloads and
recorded measurements are described in [PERFORMANCE.md](PERFORMANCE.md).

## Authoring components

Initialize the template from a Perry-WIT flake reference:

```sh
perry_wit_source="git+file:///absolute/path/to/perry-wit"
nix flake init -t "$perry_wit_source"
```

Set `inputs.perry-wit.url` in the generated `flake.nix` to an exact compiler
revision. The template shares `entry`, `wit`, and `world` between
`lib.<system>.buildComponent` and `mkSdkShell`. Both generate declarations before
type checking; shell entry preserves authored configuration.

```sh
nix develop --no-write-lock-file
tsc --noEmit -p .perry/types
nix build --no-write-lock-file
```

Node tests import an export-based component's module and call its exports.
Application WIT dependencies need application-owned Node test bindings.

Commit source, application WIT, and configuration. `.perry` is generated and
ignored. Projects using only an exact Perry revision inherit its dependency pins
and need no redundant consumer lockfile; independently selected dependencies
still need their own reproducible configuration.

SDK generation can also run explicitly:

```sh
perry-wit gen-types --wit wit --world task --entry src/index.ts
tsc --noEmit -p .perry/types
perry-wit src/index.ts --wit wit --world task -o dist/task.wasm
```

The generated check configuration extends the project's configuration and checks
the selected entry against WIT. Explicit generation creates a missing
`tsconfig.json`; `--no-tsconfig`, used by the SDK shell, leaves authored files
alone.

## Language and ownership

The source contract uses declared records, typed dictionaries, dense homogeneous
arrays, finite unions, and validated JSON value trees. Context is read-only.
Date uses immutable UTC operations; supported Temporal operations provide explicit
ISO timestamp and calendar calculations. Dynamic property deletion, arbitrary
coercion, and other compatibility work are recorded in
[TODOs.md](TODOs.md#complexity-deferred-until-a-consumer-requires-it).

Stored async tasks retain their outcomes for repeated observation.
`Promise.all`, `allSettled`, and `race` register each operand once; execution is
eager to the first suspension. Race losers continue running and must finish
before the owning call returns. Unresolved ordinary work at that boundary traps.
Standard `fetch` accepts URLs or `Request` values, typed options, header
records/pairs/`Headers`, and string or byte bodies. It resolves at response headers,
supports redirect modes, and exposes response metadata and single-consumption
`text()`, `json()`, `bytes()`, and `arrayBuffer()` methods. Bodies grow incrementally
and wait for P3 transfer completion; uploads retain snapshots through partial writes.
`Request`, `Response`, and `Headers` support construction and buffered body values.
Headers support `get`, `has`, `append`, `set`, and `delete`; fetched response headers
are immutable. Host cancellation drains HTTP owners before acknowledging the call.
AbortSignal and Web Streams remain implementation work;
the capability catalog records supported shapes and executable evidence.

## Contributing

Develop inside `nix develop`, add independent fixtures for behavior changes, and
run formatting, Cargo, strict Clippy, and applicable Nix checks. Keep support
claims tied to executable evidence in the capability catalog.
