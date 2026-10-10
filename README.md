# perry-wit

Perry-WIT compiles a static subset of TypeScript to WASI 0.3 WebAssembly components through
[Perry](https://www.perryts.com) HIR and typed MIR built on [pliron](https://github.com/pliron-org/pliron).
The linker includes the runtime services each component uses. Small identity and append components
have frozen size budgets and run with one initial 64 KiB memory page.

The compiler supports closed typed aggregates, persistent module state, checked JSON conversion,
UTF-8 strings with Unicode scalar indexing, synchronous and asynchronous WIT calls, and stackless P3
continuations. Settled awaits resume inline. Collectible storage reclaims unproven allocation lifetimes;
proven local records and stream buffers can reuse storage.

Implementation is in progress. [TODOs.md](TODOs.md) separates completed work, remaining gaps and accepted
deviations. Async module initialization and final memory/cancellation acceptance remain incomplete. [ARCHITECTURE.md](ARCHITECTURE.md) describes representations and ownership.

## Use

For compiler development, enter the pinned development shell:

```sh
nix develop
```

This environment supplies Rust 1.98.1, Wasmtime 49.0.2, Node.js, TypeScript and `wasm-tools`.
Cargo tests and the Nix CLI share the exact Wasmtime version; the CLI uses hash-pinned upstream binaries.
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

[examples/merge_docs.ts](examples/merge_docs.ts) fetches two JSON documents concurrently and returns their
merged object. Its integration test uses the checked-in `merge-docs` world and an in-process HTTP host,
including the declared structured error result.

You can also run the examples directly in Node.js without compilation:

```sh
node -e 'import("./examples/merge_task.ts").then(m => console.log(m.runTask("hello")))'
```

### Test

Run the standard test suite and linters:

```sh
# Run unit and integration tests
CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --workspace -j 1 -- --test-threads=1

# Linting and formatting checks
cargo clippy --workspace --all-targets -j 1 -- -D warnings
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
- **Tasks**: Every export call owns its scheduler and active operations. Precise collection reclaims unreachable values; module state persists across calls. Boundary owners remain live until lifting or transfer acknowledgement.
- **Async exports**: an `async func` export is implemented by an `async` TypeScript function returning `Promise<T>`.
- **Canonical values**: Records, dense lists, tuples, options, enums, finite variants and tagged results cross imports and exports. WIT `s64` and `u64` use exact `bigint` transport.
- **Results**: Explicit `{ ok: true, value }` / `{ ok: false, error }` returns take precedence. Completion-style implementations return the success payload and throw a compatible error payload.
- **Byte inputs**: Recursive `WitInput<T>` accepts `string | Uint8Array` for outbound `list<u8>`. Strings use their existing UTF-8 storage. Inbound byte lists remain mutable `Uint8Array` values.
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

[crates/conformance/deferred.txt](crates/conformance/deferred.txt) lists contracts outside the accepted domain, each with the deviation it records; they are not run. The accepted profile is listed in [TODOs.md](TODOs.md). Campaign reports record the checked source identity and each contract outcome.

Scaling contracts (`wasi.execution.scaling.*` in [crates/conformance/src/scaling.rs](crates/conformance/src/scaling.rs)) bound guest fuel growth between inputs n and 4n.

## Performance

Component budgets are frozen in [bench/baseline](bench/baseline). Integration tests exercise bounded
allocation churn, retained graphs, stream-buffer reuse and compilation scaling. Conformance scaling
contracts measure guest fuel for controlled workloads; application timings remain a separate measure.
Allocator comparisons are in [bench/allocators](bench/allocators).
For serial consumer campaigns, paired replay and direct component timing, see
[bench/performance](bench/performance). `--keep-names` retains linked function names for
Wasmtime guest profiles; normal component output remains stripped.

## Compilation Pipeline

Perry-WIT compiles TypeScript ahead-of-time directly into native WebAssembly instructions without an embedded JavaScript engine:

```text
TypeScript ──► Perry HIR ──► perry MIR (pliron)           crates/perry-lower, crates/perry-mir
                                 │  mem2reg, DCE, CFG simplification
                                 ▼
                          wasm dialect (pliron)           crates/perry-codegen
                                 │  + canonical-ABI glue for the WIT world
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

Perry-WIT supports a strictly typed subset of TypeScript designed for ahead-of-time compilation. [TODOs.md](TODOs.md) records the accepted profile and remaining gaps. Currently supported:

- **Numbers** are IEEE doubles with ECMAScript arithmetic, including `%`, `-0` and NaN. WIT integer and `f32` values convert at the boundary.
- **Records**: interfaces, type literals and object literals with closed shapes. Structurally equal types are the same record, and recursive interfaces are supported.
- **Arrays** are dense and homogeneous. An out-of-range or non-integral read yields `undefined`. Writing at `length` appends; any other out-of-range write throws a `RangeError`.
- **Strings** use UTF-8 storage with a cached Unicode scalar `.length`. Repeated appends reuse spare buffer capacity, giving amortized linear copying for a growing string. Retained prefixes remain immutable. Index semantics match Node for BMP scalars, the differential conformance domain; astral characters count as one scalar and surrogate halves are rejected.
- **Exceptions**: a `TypeError` from `undefined` at a typed use, or a `RangeError`, is catchable. Nothing in the subset traps, except an exception escaping an export that has no error type.
- **Control flow**: `if`, loops, `break`/`continue`, `&&`/`||`, `?:`, and direct calls, including recursion.
- **Async**: `async`/`await` on the WASI 0.3 callback ABI; an `await` of a settled promise continues inline, without a microtask queue. `Promise.all`, `race` and `allSettled`.
- **Byte streams**: default readers, async iteration and BYOB readers with transferred buffers, offsets, minimum reads and cancellation. Acknowledged writes preserve bounded buffer reuse.
- **Temporal**: `Instant.from(iso)` and `fromEpochMilliseconds(number)`, `PlainDate.from(iso)` and `PlainDateTime.from(iso)`, ISO `toString()`/`toJSON()`, calendar/time fields and literal `.add({ days })` on plain values. Instants expose `epochMilliseconds`; plain values expose `year`, `month`, `day` and `dayOfWeek`, with time fields through `nanosecond` on `PlainDateTime`. Invalid values throw `RangeError`. The supported subset uses the ISO calendar; options, other duration units, named-zone APIs and `Temporal.Now` receive compiler diagnostics.
- **Standard library**: `Math`, `Date` (UTC), string methods, `Uint8Array`, `TextEncoder`/`TextDecoder`, `JSON` (typed `stringify`; no revivers, replacers or indentation), `crypto` random values, `node:timers/promises`, `node:fs/promises` (`readFile`, `writeFile`, `stat`, `readdir`, `mkdir`, `unlink`, `rmdir`), `Headers`, constructed `Response`/`Request` with byte bodies, and `Writable.toWeb(process.stdout)`.

Native-record JSON object key order is determined by the closed schema.

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

Develop inside `nix develop` to ensure matching toolchain versions across dependencies.
Before submitting changes, ensure all formatting, lints, tests, and flake checks pass:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -j 1 -- -D warnings
CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=1 cargo test --workspace -j 1 -- --test-threads=1
cargo run -p perry-conformance -- check
nix flake check
```

Register each claimed behavior, input domain, and directed boundary witness in [the contract registry](crates/conformance/src/registry.rs).
