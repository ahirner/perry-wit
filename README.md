# perry-wit

**Perry-WIT** compiles static TypeScript ahead-of-time directly into lightweight, high-performance WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a heavy JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through Perry HIR and WAFFLE SSA straight to native WebAssembly. This produces self-contained components ranging from **~10 KB** for pure compute to **tens of KB** when using filesystem operations and HTTP streaming—with instant startup times, low linear memory usage, and zero runtime bloat.

- **Dual-mode authoring**: Write standalone CLI commands using top-level `await`, export typed functions matching a WIT interface, or combine both in a single source file.
- **Node-compatible**: Share code seamlessly between Node.js test harnesses and native Wasm components using standard static ESM imports and type definitions.
- **Hermetic toolchain**: Fully reproducible builds, type generation, and component testing via Nix flakes.

For runtime boundaries, memory layouts, and compilation pipeline details, see [ARCHITECTURE.md](ARCHITECTURE.md). For verified capability contracts, see the [capability catalog](catalog/capabilities.json).

## Environment

For compiler development, enter the repository's pinned environment:

```sh
nix develop
```

This supplies Rust, Node.js, TypeScript, Wasmtime, and `wasm-tools`.
- `WASI_WIT_PATH` and `WASI_P3_WIT_PATH` identify the official WASI 0.3 packages.
- Application WIT dependencies in `wit/deps` automatically take precedence over ambient packages.

For component development outside this checkout, use [the starter template](template/README.md). In this repository, `nix develop .#sdk` generates the SDK for `examples/merge_task.ts`.

## Usage

### Build

Build components and compiler tools with Nix:

```sh
# Build the perry-wit CLI compiler
nix build .#perry-wit

# Build example components
nix build .#example-merge-task
nix build .#template-component
```

Or build locally with Cargo inside the development shell:

```sh
cargo build --release
```

### CLI Scripts

A world exporting `wasi:cli/run@0.3.0` turns top-level statements and top-level `await` into a command component:

```ts
// example.ts
import { setTimeout } from "node:timers/promises";

await Promise.all([setTimeout(2), setTimeout(1)]);
console.log("done");
```

Run directly in Node, or compile and run in Wasmtime P3:

```sh
# Run directly with Node
node example.ts

# Compile to a WASI 0.3 command component
cargo run -- example.ts --wit wit --world command -o dist/example.wasm

# Execute with Wasmtime P3
wasmtime run -C cache=n -S p3=y \
  -W component-model-async=y -W component-model-async-stackful=y \
  -W component-model-more-async-builtins=y -W component-model-threading=y \
  dist/example.wasm
```

Use `process.exitCode = 1` to set the exit status when execution finishes, or call `process.exit(1)` to terminate immediately.

### Text and resource boundaries

Outbound WIT `list<u8>` values accept `string | Uint8Array`. This applies recursively to import arguments and exported results, including records, options, variants, and lists. Incoming byte lists remain mutable `Uint8Array` values. The generated SDK provides `WitInput<T>` and named `TInput` aliases for outbound values:

```ts
import { submit, type DocumentInput } from "example:documents/store";

const document: DocumentInput = { bytes: JSON.stringify({ message: "Grüße" }) };
submit(document);
```

Strings lower using their existing UTF-8 pointer and byte length, with their storage retained for the call. No intermediate encoding buffer is created. Component-to-component transfer may still copy into the receiving memory. Explicit `TextEncoder.encode` returns an independent, mutable copy.

Opaque interface resources have nominal SDK types. An imported resource `secret` exposes `dropSecret`; an exported resource additionally exposes `newSecret(representation)` and `secretRep(secret)`. Transferring or disposing ownership invalidates all aliases. Borrowed handles expire when the exporting call returns and cannot be disposed or transferred as owned handles. Dispose owned imports explicitly, normally in `finally`:

```ts
const result = await get("api-key");
if (!result.ok) throw new Error("secret unavailable");
const secret = result.value;
try { return await reveal(secret); }
finally { dropSecret(secret); }
```

Resource constructors and methods declared in WIT still require freestanding wrapper functions. Numeric `BigInt(value)` construction supports finite safe integers for WIT `u64`/`s64` fields; arbitrary-precision arithmetic and bigint literals are not implemented.

### Library Exports

When implementing WIT interface exports, WIT is authoritative for names, parameter/result types, and async effects. An export such as `run-task: func(input: string) -> string` is implemented as:

```ts
export function runTask(input: string): string {
  return "received: " + input;
}
```

Compile and invoke the exported function:

```sh
# Compile library component
cargo run -- examples/merge_task.ts --wit wit --world task-runner -o dist/task.wasm

# Invoke export via Wasmtime
wasmtime run -C cache=n -S p3=y -W component-model-async=y \
  --invoke 'run-task("hello")' dist/task.wasm
```

- **Data Types**: Both `s64` and `u64` map to TypeScript `bigint` (including nested records, lists, options, and tuples). Literal record field names remain unchanged even when matching builtins (`fetch`, `process`, `console`).
- **Initialization**: Static dependencies and top-level statements execute once before the first exported call. Module state persists across subsequent invocations.
- **Async Exports**: An export must be declared as `async func` in WIT if it can suspend, including during module initialization or through filesystem, timer, and HTTP calls.
- **Core Wasm Output**: Pass `--core-only` (or use a `.core.wasm` extension) to emit unlinked core Wasm for custom embedding.

### Test

Perry-WIT includes unit tests, differential oracle testing against Node.js, and end-to-end integration tests:

```sh
# Run unit and integration tests
cargo test --workspace

# Run differential conformance tests against the Node.js oracle
cargo test --test conformance_test

# Validate formal capability evidence and generate target/conformance/report.json
node scripts/check_conformance.mjs

# Linter and formatting checks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# End-to-end HTTP integration and hermetic Nix flake checks
./scripts/test_e2e.sh
nix flake check
```

The conformance check executes fresh Rust and Node tests, validates each capability against the catalog's exact test identifiers, and outputs the audited report to `target/conformance/report.json`.

## Authoring Components

Initialize a new TypeScript component from the template:

```sh
nix flake init -t git+file:///path/to/perry-wit
```

1. **Define WIT**: Specify your component's world and dependencies in `wit/world.wit`.
2. **Generate Types**: Run `nix develop` or generate TypeScript declarations manually:
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

## Semantics

Perry-WIT supports a predictable, strictly typed subset of TypeScript designed for high-performance ahead-of-time compilation.

### Types
- **Supported Shapes**: Declared interfaces/records, typed dictionaries, dense homogeneous arrays, finite unions, and validated JSON trees.
- **64-bit Integers**: Both `s64` and `u64` use TypeScript `bigint`.
- **Dates**: `Date` supports immutable UTC operations, while supported `Temporal` APIs provide explicit ISO timestamp and calendar calculations.
- **Unsupported Dynamic Features**: Arbitrary runtime coercion, dynamic property deletion (`delete obj.prop`), and untyped prototype manipulation are rejected at compile time.

### Concurrency
- **Scheduling**: Execution runs eagerly until the first suspension point. Continuations are scheduled directly via native WASI 0.3 wakeups.
- **Combinators**: `Promise.all`, `Promise.allSettled`, and `Promise.race` register each operand once. Stored async tasks retain their outcomes for repeated observation.
- **Race Semantics**: Non-winning branches in `Promise.race` continue running in the background and must settle or be cancelled before the owning call returns. Unresolved dangling work at that boundary traps.

### I/O
- **HTTP Fetch**: Standard `fetch` accepts URLs or `Request` instances, typed options, header dictionaries/pairs/`Headers`, and string or byte bodies. It resolves at response headers, supports redirect modes, and exposes metadata alongside single-consumption `text()`, `json()`, `bytes()`, and `arrayBuffer()` methods.
- **Byte Streams**: `Request` and `Response` bodies expose byte-stream readers with locking, cancellation, and `for await` iteration.
- **Cancellation**: `AbortController` and `AbortSignal` cancel in-flight HTTP requests, pending stream readers, and filesystem transfers.
- **Filesystem**: Asynchronous I/O via `node:fs/promises` (`readFile` and `writeFile` with `AbortSignal` support). Synchronous and callback-based `fs` APIs receive compile-time diagnostics.
- **Timers**: Promise-based timers via `setTimeout` from `node:timers/promises`.

### Resource Management
Host resources—such as open files, HTTP connections, and stream buffers—are managed directly by the host runtime. Perry-WIT preserves these resources across asynchronous suspensions and frees them automatically when an operation finishes or when an `AbortSignal` triggers cancellation. If an unhandled trap or abnormal exit occurs, the host reclaims all active resources cleanly during store disposal.

## Contributing

Develop inside `nix develop` to ensure matching toolchain versions across dependencies.

Before submitting changes, ensure all formatting, lints, tests, and flake checks pass:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node scripts/check_conformance.mjs
nix flake check
```

Keep capability claims synchronized with executable evidence in [catalog/capabilities.json](catalog/capabilities.json).
