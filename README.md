# perry-wit

**Perry-WIT** compiles static TypeScript ahead-of-time directly into lightweight, high-performance WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a heavy JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and a typed middle IR built on [pliron](https://github.com/pliron-org/pliron) straight to native WebAssembly. A string-processing component is under **3 KB**, including its runtime, with instant startup and memory released in bulk after every call.

> **Status: compiler rewrite in progress.** The WAFFLE-based backend has been removed. The MIR pipeline currently covers exports over numbers, booleans, strings, dense arrays and records, with catchable exceptions and linear-time memory. It also covers async functions on the WASI P3 callback ABI with `setTimeout` from `node:timers/promises`, `throw`/`try`/`catch`/`finally`, CLI commands with top-level `await`, `console`, `process.env`, `process.exit` and the clocks, and the core standard library: `Math`, `Date`, string methods, `Uint8Array`, `TextEncoder`/`TextDecoder`, `JSON`, `Promise.all`/`race`/`allSettled`/`resolve`/`reject`, `Error` causes and `crypto` random values, plus `node:fs/promises`, `fetch` over `wasi:http/client`, `Headers`/`Response`/`Request` (with `json()`), object spreads of JSON values, array destructuring, and `ReadableStream`/`WritableStream` over `stream<u8>` and stdio, and HTTP handlers (`wasi:http/handler`). `AbortController` and `Temporal` return with the feature migration in [TODOs.md](TODOs.md) (P6.2). [crates/conformance/passing.txt](crates/conformance/passing.txt) lists the contracts that pass today.

- **WIT-first authoring**: Export typed functions matching a WIT world, or write a standalone CLI command with top-level `await`.
- **Node-compatible**: Share code seamlessly between Node.js test harnesses and native Wasm components using standard static ESM imports and type definitions.
- **Hermetic toolchain**: Fully reproducible builds, type generation, and component testing via Nix flakes.

For the pipeline, memory tiers and component boundary, see [ARCHITECTURE.md](ARCHITECTURE.md); for the plan, see [TODOs.md](TODOs.md).

## Use

For compiler development, enter the pinned development shell:

```sh
nix develop
```

This environment supplies Rust, Node.js, TypeScript, Wasmtime, and `wasm-tools`.
- `WASI_WIT_PATH` identifies official WASI 0.3 packages.
- Application WIT dependencies in `wit/deps` automatically take precedence over ambient packages.

For component development outside this checkout, use [the starter template](template/README.md). In this repository, `nix develop .#sdk` generates the SDK for [examples/merge_task.ts](examples/merge_task.ts).

### Run Examples

The repository includes ready-to-run examples.

#### Library Component

[examples/merge_task.ts](examples/merge_task.ts) exports functions matching the `task-runner` WIT world:

```sh
# Compile to a WASI 0.3 component
cargo run -- examples/merge_task.ts --wit wit --world task-runner -o dist/merge_task.wasm

# Invoke the exported run-task function with Wasmtime
wasmtime run -C cache=n -S p3=y -W component-model-async=y \
  --invoke 'run-task("hello world")' dist/merge_task.wasm
```

[examples/merge_docs.ts](examples/merge_docs.ts) (`fetch`, `Promise.all`, `json()`, an object spread of JSON values) compiles to a 49,634-byte component (114,105 on the legacy backend); `tests/fetch_test.rs` runs it against a local server.

You can also run the examples directly in Node.js without compilation:

```sh
node -e 'import("./examples/merge_task.ts").then(m => console.log(m.runTask("hello")))'
```

### Test

Run the standard test suite and linters:

```sh
# Run unit and integration tests
cargo test --workspace

# Linting and formatting checks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# Complete Nix flake checks
nix flake check
```

For formal capability and semantic verification against the Node.js oracle, see [Conformance](#conformance).

### Build

Build components and compiler tools using Cargo or Nix:

```sh
# Build the perry-wit compiler CLI with Cargo
cargo build --release

# Build compiler CLI with Nix
nix build .#perry-wit

# Build example components with Nix
nix build .#example-merge-task
nix build .#template-component
```

Output binaries are placed in `target/release/perry-wit` or `result/bin/`.

## Authoring

Components can be authored as libraries exporting WIT functions, standalone CLI scripts, or complete projects based on the starter template.

### Library Components

When implementing WIT interface exports, WIT is authoritative for function names, parameter and return types, and async effects.

Kebab-case WIT export names map automatically to camelCase in TypeScript. For example, `export run-task: func(input: string) -> string` in `world.wit` is implemented as:

```ts
export function runTask(input: string): string {
  return "received: " + input;
}
```

Compile and invoke the library export:

```sh
cargo run -- src/index.ts --wit wit --world task-runner -o dist/component.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("test")' dist/component.wasm
```

- **Exports**: Exported TypeScript functions not named by the world stay internal, so one source can implement several worlds.
- **Tasks**: Every export call is one task. Everything it allocates, including its arguments and results, is released in bulk after the host has read the result.
- **Async exports**: an `async func` export is implemented by an `async` TypeScript function returning `Promise<T>`.
- **Not yet migrated**: module-level state and initialization, WIT imports other than `wasi:clocks/monotonic-clock` for timers, and `option`, `result`, `variant`, `enum`, `flags`, `tuple` and resource types at the export boundary. Programs can use the SDK's generated types for records, variants (`{ tag, val }`), options and tuples, as HTTP handlers do.
- **Core Wasm Output**: Pass `--core-only` to emit the linked core module without component encoding.

### CLI Scripts

A module with top-level statements compiles against a world that exports `wasi:cli/run@0.3.0` (the `command` world in [wit/world.wit](wit/world.wit)); its statements, including top-level `await`, become `run`.

```sh
perry-wit --world command -o hello.wasm hello.ts
wasmtime run -S p3=y -W component-model-async=y hello.wasm
```

- `console.log`/`info`/`debug` write to stdout and `console.error`/`warn` to stderr, one line per call with primitive arguments.
- `process.exitCode = 1` or `process.exit(1)` fails the command; `process.exit` returns without running `catch` or `finally`.
- An uncaught exception prints `Uncaught <error>` to stderr and fails the command.
- `process.env.NAME`, `Object.keys(process.env)`, `performance.now()` and `Date.now()` read the WASI environment and clocks.
- Console output needs async exports: it drains through WASI P3 streams, which a synchronous export cannot wait for.

### HTTP Handlers

A program implementing the `handler` world of [wit/http-handler](wit/http-handler/world.wit) compiles to a component that exports `wasi:http/handler@0.3.0`. It receives the request as a plain record (method and scheme variants, authority, path, headers as `[name, bytes]` pairs, the whole body) and returns `{ status, headers, body }`; `handle` may be `async`.

```sh
perry-wit gen-types --wit wit/http-handler --world handler -o handler-types --no-tsconfig
perry-wit --wit wit/http-handler --world handler -o handler.wasm handler.ts
wasmtime serve -S p3=y -W component-model-async=y handler.wasm
```

```ts
import type { PerryHttpHandlerTypesRequest as Request, PerryHttpHandlerTypesResponse as Response } from './handler-types/world';

export function handle(request: Request): Response {
  const body = new TextEncoder().encode('hello ' + request.pathWithQuery);
  return { status: 200, headers: [['content-type', new TextEncoder().encode('text/plain')]], body };
}
```

- One instance serves many requests; each is its own task, released when its response body was sent.
- An exception (or an invalid response header) returns `internal-error` with the error's description (`TypeError: …`) instead of trapping, so the instance keeps serving.
- Request and response bodies are read and written whole.

### Project Setup

To start a new component project from the bundled template:

```sh
nix flake init -t git+file:///path/to/perry-wit
```

1. **Define WIT**: Specify the component world and dependencies in `wit/world.wit`.
2. **Generate Types**: Run `nix develop` or invoke `gen-types` manually:
   ```sh
   perry-wit gen-types --wit wit --world task --entry src/index.ts
   ```
   This generates exact definitions in `.perry/types/`.
3. **Type Check**: Validate your implementation against the WIT contract:
   ```sh
   tsc --noEmit -p .perry/types
   ```
4. **Compile Component**:
   ```sh
   perry-wit src/index.ts --wit wit --world task -o dist/task.wasm
   ```

## Conformance

Perry-WIT includes an executable conformance verification framework in [crates/conformance](crates/conformance) that differentially tests Perry-WIT against Node.js as the runtime oracle.

```sh
# Execute all capability contracts and directed tests
cargo run -p perry-conformance -- check

# List registered capability contracts
cargo run -p perry-conformance -- list

# Select specific capability groups
cargo run -p perry-conformance -- check --select node.fs
```

All verified capability contracts are declared in [crates/conformance/src/registry.rs](crates/conformance/src/registry.rs).

[crates/conformance/passing.txt](crates/conformance/passing.txt) is a ratchet:
- A listed contract that fails is a regression and fails the campaign.
- An unlisted contract that fails is reported as `pending` and does not fail the campaign.
- An unlisted contract that passes fails the campaign until it is added, so the list only grows.

[crates/conformance/deferred.txt](crates/conformance/deferred.txt) lists contracts outside the accepted domain, each with the deviation it records; they are not run. The accepted deviations are listed in [TODOs.md](TODOs.md) (P6.2). Status: 45 contracts pass, 4 are deferred and 1 is pending.

Scaling contracts (`wasi.execution.scaling.*` in [crates/conformance/src/scaling.rs](crates/conformance/src/scaling.rs)) bound guest fuel growth between inputs n and 4n.

## Performance

The legacy benchmark harness was removed with the legacy backend; its last numbers are kept in [TODOs.md](TODOs.md) (P0.2) as the baseline for the migrated stream and HTTP workloads. Today, size and fuel are measured by the scaling contracts, `tests/mir_codegen_test.rs` and `tests/component_test.rs`. Allocator choices are measured in [bench/allocators](bench/allocators).

## Compilation Pipeline

Perry-WIT compiles TypeScript ahead-of-time directly into native WebAssembly instructions without an embedded JavaScript engine:

```text
TypeScript ──► Perry HIR ──► perry MIR (pliron)           crates/perry-lower, crates/perry-mir
                                 │  mem2reg, DCE, CFG simplification
                                 ▼
                          wasm dialect (pliron)           crates/perry-codegen
                                 │  + canonical-ABI glue for the WIT world's exports
                                 ▼
                     structured core Wasm                 crates/pliron-wasm (emitter)
                                 │  + perry-runtime and helper libraries (PIC, linked)
                                 ▼
                      WASI 0.3 component                  wit-component ComponentEncoder
```

1. **Builder**: lowers Perry HIR to MIR. Every local is a slot; pliron's `mem2reg` builds SSA. Records are identified by shape, and unsupported constructs are rejected with a diagnostic.
2. **Codegen**: translates MIR to the `wasm` dialect, chooses value representations, and emits the export glue. Strings and lists are passed in UTF-8 and the canonical layout without transcoding.
3. **Emitter**: structures reducible control flow with Ramsey's dominator-tree algorithm, keeps single-use values on the operand stack, and encodes with `wasm-encoder`.
4. **Link and encode**: relocates `perry-runtime` (memory tiers, strings, arrays) and the computation helpers into the module, embeds the WIT world, and encodes the component.

## Semantics

Perry-WIT supports a predictable, strictly typed subset of TypeScript designed for ahead-of-time compilation. The subset rules R1–R8 are in [TODOs.md](TODOs.md) (D4). Currently supported:

- **Numbers** are IEEE doubles with ECMAScript arithmetic, including `%`, `-0` and NaN. WIT integer and `f32` values convert at the boundary.
- **Records**: interfaces, type literals and object literals with closed shapes. Structurally equal types are the same record, and recursive interfaces are supported.
- **Arrays** are dense and homogeneous (R8). An out-of-range or non-integral read yields `undefined`. Writing at `length` appends; any other out-of-range write throws a `RangeError`.
- **Strings** are UTF-8 with an O(1) UTF-16 `.length`. Concatenation in a loop is linear. Index semantics match Node for BMP scalars, the conformance domain; there are no surrogate halves.
- **Exceptions**: a `TypeError` from `undefined` at a typed use, or a `RangeError`, is catchable. Nothing in the subset traps, except an exception escaping an export that has no error type.
- **Control flow**: `if`, loops, `break`/`continue`, `&&`/`||`, `?:`, and direct calls, including recursion.
- **Async**: `async`/`await` on the WASI 0.3 callback ABI; an `await` of a settled promise continues inline, without a microtask queue. `Promise.all`, `race` and `allSettled`.
- **Standard library**: `Math`, `Date` (UTC), string methods, `Uint8Array`, `TextEncoder`/`TextDecoder`, `JSON` (typed `stringify`; no revivers, replacers or indentation), `crypto` random values, `node:timers/promises`, `node:fs/promises` (`readFile`, `writeFile`, `stat`, `readdir`, `mkdir`, `unlink`, `rmdir`), `Headers`, constructed `Response`/`Request` with byte bodies, and `Writable.toWeb(process.stdout)`.

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

Develop inside `nix develop` to ensure matching toolchain versions across dependencies.
Before submitting changes, ensure all formatting, lints, tests, and flake checks pass:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p perry-conformance -- check
nix flake check
```

Register each claimed behavior, input domain, and directed boundary witness in [the contract registry](crates/conformance/src/registry.rs).
