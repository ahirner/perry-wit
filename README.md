# perry-wit

**Perry-WIT** compiles static TypeScript ahead-of-time directly into lightweight, high-performance WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a heavy JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and a typed middle IR built on [pliron](https://github.com/pliron-org/pliron) straight to native WebAssembly. A string-processing component is under **3 KB**, including its runtime, with instant startup and memory released in bulk after every call.

> **Status: compiler rewrite in progress.** The WAFFLE-based backend has been removed. The MIR pipeline currently covers exports over numbers, booleans, strings, dense arrays, records and Uint8Array byte views, with `try`/`catch`/`finally` and task-scoped memory. JSON variant trees support parsing, compact serialization and checked narrowing. It also covers async functions on the WASI P3 callback ABI, with `setTimeout` from `node:timers/promises`, cancellation, and CLI execution, console output, environment/arguments, clocks and process exit. `fetch`, `node:fs`, general streams, remaining JSON operations, text codecs, Date objects and HTTP handlers return with the feature migration in [TODOs.md](TODOs.md) (P6.2). [crates/conformance/passing.txt](crates/conformance/passing.txt) lists the contracts that pass today.

- **WIT-first authoring**: Export typed functions matching a WIT world, or execute top-level code and `await` through a `wasi:cli/run` command world.
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

[examples/merge_docs.ts](examples/merge_docs.ts) (`fetch`, `Promise.all`, JSON) is the target for the async and HTTP migration and does not compile yet.

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
- **Not yet migrated**: module-level state and initialization, general WIT function imports, and `option`, `result`, `variant`, `enum`, `flags`, `tuple` and resource types.
- **Core Wasm Output**: Pass `--core-only` to emit the linked core module without component encoding.

### CLI Scripts

Worlds exporting `wasi:cli/run@0.3.0` execute top-level statements and `await`, followed by an exported SDK command implementation when present (`runRun` for the standard command world). Its `{ok:true}` or `{ok:false}` result becomes the command result; uncaught exceptions return a command error. `console.log` and `console.error` currently accept one string; their P3 streams drain before completion. `process.env.NAME`, `Object.keys(process.env)`, host-provided `process.argv`, `performance.now()` and `Date.now()` use the corresponding WIT imports.

Assign a safe integer to `process.exitCode` for graceful termination after pending work and output drain. Assigning `undefined` or `null` clears it. `process.exit(code)` terminates immediately, cancels pending native operations, and bypasses catches and finalizers; pending output may be discarded. Omitting the code uses the assigned status or zero. Numeric exit codes are reduced modulo 256 at the WASI boundary; string codes are not supported yet.

```sh
cargo run -- examples/cli.ts --wit wit --world command -o dist/cli.wasm
wasmtime run -S p3=y -W component-model-async=y dist/cli.wasm
```

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
- **Strings** are UTF-8 with an O(1) UTF-16 `.length`. Concatenation in a loop is linear.
- **JSON** uses the explicit recursive union `null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue }`. Tree access and mutation preserve object/array aliases; compact serialization preserves key order and Unicode text. Stored native records and arrays serialize directly and can share their storage through JSON views; writes through those views must respect the native types. Scalars pass checked boundaries into native typed code; typed record and homogeneous-array views retain parsed storage, identity and mutations. Typed reads check values again after mutation through a JSON alias. Invalid conversions throw `TypeError`, and invalid JSON syntax throws `SyntaxError`. Parsed objects support key insertion/deletion, `Object.keys` snapshots and `Object.hasOwn`; present `undefined` values remain distinct from absent properties. Nested `undefined` serializes by omission in objects and as null in arrays. Native records retain closed shapes, and deleting a dense-array element throws `RangeError`. Prototype setters are unsupported; an existing parsed `__proto__` data property remains writable. Replacers/revivers, indentation, general typed dictionaries and value/entry enumeration remain pending. Nesting is limited to 128; unpaired surrogates and JSON string indexing are unsupported.
- **Exceptions**: `try`/`catch`/`finally`, Error/TypeError/RangeError construction, `.message`, `instanceof`, and throwing numbers, booleans or strings. Rethrows and rejected promises preserve Error identity across suspension. Implicit TypeError/RangeError failures are catchable. An uncaught command exception returns `err`; an exception escaping an export without an error type traps.
- **Control flow**: `if`, loops, `break`/`continue`, `&&`/`||`, `?:`, and direct calls, including recursion.

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
