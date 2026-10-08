# perry-wit

**Perry-WIT** compiles static TypeScript ahead-of-time directly into lightweight, high-performance WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a heavy JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and [WAFFLE](https://crates.io/crates/waffle) SSA straight to native WebAssembly. This produces self-contained components ranging from **~5 KB** for pure compute to **tens of KB** when using filesystem operations and HTTP streaming—with instant startup times, low linear memory usage, and zero runtime bloat.

- **Dual-mode authoring**: Write standalone CLI commands using top-level `await`, export typed functions matching a WIT interface, or combine both in a single source file.
- **Node-compatible**: Share code seamlessly between Node.js test harnesses and native Wasm components using standard static ESM imports and type definitions.
- **Hermetic toolchain**: Fully reproducible builds, type generation, and component testing via Nix flakes.

For runtime boundaries, memory layouts, and compilation pipeline details, see [ARCHITECTURE.md](ARCHITECTURE.md).

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

The repository includes ready-to-run examples demonstrating library exports, CLI execution, and HTTP streaming.

#### Library Component

[examples/merge_task.ts](examples/merge_task.ts) exports functions matching the `task-runner` WIT world:

```sh
# Compile to a WASI 0.3 component
cargo run -- examples/merge_task.ts --wit wit --world task-runner -o dist/merge_task.wasm

# Invoke the exported run-task function with Wasmtime
wasmtime run -C cache=n -S p3=y -W component-model-async=y \
  --invoke 'run-task("hello world")' dist/merge_task.wasm
```

#### Async HTTP Component

[examples/merge_docs.ts](examples/merge_docs.ts) uses standard `fetch` with `Promise.all` to fetch and combine JSON documents concurrently:

```sh
# Compile the document merger component
cargo run -- examples/merge_docs.ts --wit wit --world merge-docs -o dist/merge_docs.wasm

# Invoke with Wasmtime and HTTP capability enabled
wasmtime run -C cache=n -S p3=y -S http=y \
  -W component-model-async=y -W component-model-async-stackful=y \
  -W component-model-more-async-builtins=y -W component-model-threading=y \
  --invoke 'merge-docs()' dist/merge_docs.wasm
```

You can also run both examples directly in Node.js without compilation:

```sh
node -e 'import("./examples/merge_task.ts").then(m => console.log(m.runTask("hello")))'
```

### Test

Run the standard test suite, linters, and end-to-end integration checks:

```sh
# Run unit and integration tests
cargo test --workspace

# Linting and formatting checks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# End-to-end HTTP integration tests
./scripts/test_e2e.sh

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
nix build .#example-merge-docs
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

- **Initialization**: Static dependencies and top-level statements execute once before the first exported call. Module state persists across subsequent invocations.
- **Async Exports**: An export must be declared as `async func` in WIT if it can suspend, including through filesystem operations, timers, or HTTP calls.
- **Core Wasm Output**: Pass `--core-only` (or use a `.core.wasm` extension) to emit unlinked core WebAssembly for custom embedders.

### CLI Scripts

A world exporting `wasi:cli/run@0.3.0` (such as `world command` in `wit/world.wit`) turns top-level statements and top-level `await` into a command component:

```ts
// cli.ts
import { setTimeout } from "node:timers/promises";

await Promise.all([setTimeout(2), setTimeout(1)]);
console.log("CLI finished successfully");
```

Compile and run the command component:

```sh
# Run directly in Node
node cli.ts

# Compile to a WASI 0.3 command component
cargo run -- cli.ts --wit wit --world command -o dist/cli.wasm

# Execute with Wasmtime P3
wasmtime run -C cache=n -S p3=y \
  -W component-model-async=y -W component-model-async-stackful=y \
  -W component-model-more-async-builtins=y -W component-model-threading=y \
  dist/cli.wasm
```

Use `process.exitCode = 1` to set the exit status when execution finishes, or call `process.exit(1)` to terminate immediately.

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

## Performance

Component performance and resource utilization can be measured using the dedicated benchmark suite:

```sh
nix develop -c node scripts/compare_components.mjs
```

The benchmark runner executes [tests/component_measurement.rs](tests/component_measurement.rs) in release mode and reports:
- **Execution time**: Evaluated under current-thread Tokio in two placements: root-future await and spawned task execution.
- **Component size and memory**: Stripped component byte counts and committed Wasm linear memory after warmup.
- **Host interaction efficiency**: Counts of host poll wakeups, read transfers, worker starts, and callbacks.
- **Runtime operations**: Allocator entries, realloc calls, root frames, and active promise records.
- **BYOB streaming**: Compares caller-allocated buffers (`read(buffer, { min })`) with default chunked streaming, demonstrating reduced promise overhead and fewer host context switches.

## Compilation Pipeline

Perry-WIT compiles TypeScript ahead-of-time directly into native WebAssembly instructions without an embedded JavaScript engine:

```text
 TypeScript AST + static ESM             WIT World Definitions
          │                                        │
          │◄─── 1. Bind imports, inject command ───┤
          │        adapter before syntax erasure   │
          ▼                                        │
  2. Perry HIR Lowering                            │
          │                                        │
          │◄─── 3. Validate signatures, plan ──────┤
          │        Canonical ABI layouts           │
          ▼                                        │
  4. WAFFLE SSA Lowering                           │
          │                                        │
  5. Code Generation ─────────────────► 6. ComponentEncoder
                                       (Embed resolved WIT)
                                                   │
                                                   ▼
                                      Validated WASI 0.3 Component
```

1. **Source Analysis**: Resolves local ESM dependencies, validates AST constructs, binds WIT interface imports before syntax erasure, and synthesizes command adapters when targeting `wasi:cli/run`.
2. **HIR Lowering**: Lowers the normalized AST into Perry HIR, establishing lexical scopes and guarded top-level module initialization.
3. **Contract Resolution**: Validates HIR function signatures against WIT exports, verifies that suspending functions are declared `async func`, and derives canonical ABI memory layouts and capability plans.
4. **SSA Lowering**: Lowers HIR into typed SSA basic blocks. Heap references are tracked in root frames across async suspension points, while complex algorithmic tasks (such as JSON codecs and date arithmetic) are statically linked from compact, allocator-free Rust helpers.
5. **Code Generation**: WAFFLE optimizes SSA control flow graphs, enforces reducibility, and reconstructs structured core WebAssembly instructions.
6. **Component Encoding**: Synthesizes canonical ABI import and export adapters, embeds resolved WIT interface metadata via `wit_component::ComponentEncoder`, and emits the validated WASI 0.3 component.

For memory layouts, coroutine scheduling, and helper linking details, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Semantics

Perry-WIT supports a predictable, strictly typed subset of TypeScript designed for high-performance ahead-of-time compilation.

### Types
- **Shapes**: Declared interfaces and records, typed dictionaries, dense homogeneous arrays, finite unions, and validated JSON trees. Literal record field names are preserved even when matching builtins (`fetch`, `process`, `console`).
- **Integers**: Both `s64` and `u64` map to TypeScript `bigint` (including nested records, lists, options, and tuples). `BigInt(value)` construction supports finite safe integers, while arbitrary-precision arithmetic and bigint literals are not implemented.
- **Strings**: Lower directly using their UTF-8 pointer and byte length without intermediate re-encoding.
- **Bytes**: Outbound WIT `list<u8>` accepts `string | Uint8Array` recursively across import arguments and exported results (records, options, variants, lists). The generated SDK provides `WitInput<T>` and named `TInput` aliases for outbound values. Incoming byte lists remain mutable `Uint8Array` values.
- **Dates**: `Date` supports immutable UTC operations, while supported `Temporal` APIs provide explicit ISO timestamp and calendar calculations.
- **Coercion**: Arbitrary runtime coercion, dynamic property deletion (`delete obj.prop`), and untyped prototype manipulation are rejected at compile time.

### Concurrency
- **Scheduling**: Execution runs eagerly until the first suspension point. Continuations are scheduled directly via native WASI 0.3 wakeups. Uncontended settled promises bypass host yields for immediate synchronous continuation.
- **Combinators**: `Promise.all`, `Promise.allSettled`, and `Promise.race` register each operand once. Stored async tasks retain their outcomes for repeated observation.
- **Race Semantics**: Non-winning branches in `Promise.race` continue running in the background and must settle or be cancelled before the owning call returns. Unresolved dangling work at that boundary traps.

### I/O
- **HTTP Fetch**: Standard `fetch` accepts URLs or `Request` instances, typed options, header dictionaries, pairs, or `Headers`, and string or byte bodies. It resolves at response headers, supports redirect modes, and exposes metadata alongside single-consumption `text()`, `json()`, `bytes()`, and `arrayBuffer()` methods.
- **Byte Streams**: `Request` and `Response` bodies expose byte-stream readers with locking, cancellation, BYOB reading, and `for await` iteration.
- **Cancellation**: `AbortController` and `AbortSignal` cancel in-flight HTTP requests, pending stream readers, and filesystem transfers.
- **Filesystem**: Asynchronous I/O via `node:fs/promises` (`readFile` and `writeFile` with `AbortSignal` support). Synchronous and callback-based `fs` APIs receive compile-time diagnostics.
- **Timers**: Promise-based timers via `setTimeout` from `node:timers/promises`.
- **Resource Cleanup**: Active streams, open files, and network operations are reclaimed automatically when finished, when cancelled via `AbortSignal`, or upon unhandled traps.

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
