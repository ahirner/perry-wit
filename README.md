# perry-wit

**Perry-WIT** compiles static TypeScript ahead-of-time directly into lightweight, high-performance WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a heavy JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and [WAFFLE](https://crates.io/crates/waffle) SSA straight to native WebAssembly. This produces self-contained components ranging from **~10 KB** for pure compute to **tens of KB** when using filesystem operations and HTTP streaming—with instant startup times, low linear memory usage, and zero runtime bloat.

- **Dual-mode authoring**: Write standalone CLI commands using top-level `await`, export typed functions matching a WIT interface, or combine both in a single source file.
- **Node-compatible**: Share code seamlessly between Node.js test harnesses and native Wasm components using standard static ESM imports and type definitions.
- **Hermetic toolchain**: Fully reproducible builds, type generation, and component testing via Nix flakes.

For runtime boundaries, memory layouts, and compilation pipeline details, see [ARCHITECTURE.md](ARCHITECTURE.md). For verified capability contracts, see the [executable contract registry](crates/conformance/src/registry.rs).

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

- **Initialization**: Static dependencies and top-level statements execute once before the first exported call. Module state persists across subsequent invocations.
- **Async Exports**: An export must be declared as `async func` in WIT if it can suspend, including during module initialization or through filesystem, timer, and HTTP calls.
- **Core Wasm Output**: Pass `--core-only` (or use a `.core.wasm` extension) to emit unlinked core Wasm for custom embedding.

### Fetch

Use the standard global `fetch`. For ordinary JSON, text, or binary responses, consume the body with `response.json()`, `response.text()`, or `response.arrayBuffer()`. These methods read the whole body without an application byte limit.

For a hard size limit, use a BYOB reader. [merge_docs.ts](examples/merge_docs.ts) accepts only status 200 and at most 64 KiB of valid UTF-8 per document. One `read(buffer, { min: 65537 })` waits for EOF or the first excess byte; the `finally` block cancels the reader and releases its lock. The SDK supplies the standard `min` declaration for older TypeScript DOM libraries. This example uses Node 20.17+ or Perry.

The repository's `tsconfig.json` checks the examples and authored SDK declarations, excluding generated test artifacts. Run `tsc -p tsconfig.json`; ordinary TypeScript language servers use the same configuration.

### Test

Perry-WIT includes ordinary Rust tests, one executable conformance registry for directed and generated checks against Node.js, and end-to-end integration tests:

```sh
# Run unit and integration tests
cargo test --workspace

# Execute every contract, directed witness, and randomized smoke campaign
cargo run -p perry-conformance -- check

# Generate the catalog from executable contracts
cargo run -p perry-conformance -- list

# Linter and formatting checks
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# End-to-end HTTP integration
./scripts/test_e2e.sh
nix flake check
```

The development-only `perry-conformance` crate owns strategies, examples, observation checks, and reports. Each declared boundary partition has a mandatory witness. Catalogs and reports distinguish equivalent execution, expected compiler rejection, and deliberate fault detection. Node and Perry execute identical source; failures include concrete minimized inputs, every metamorphic source form, configuration, toolchain versions, and source identity under `target/conformance`. An incomplete, skipped, exhausted, or stale execution cannot establish a passing claim. Registered domains describe bounded coverage, not total ECMAScript or Node conformance.

The default smoke campaign generates eight programs per contract at depth three and runs the preserved IEEE-754 boundary corpus. Select contracts or capability groups with `--select node.fs`, extend budgets with `--cases 200 --depth 4 --seed 100`, and replay a concrete saved case with `cargo run -p perry-conformance -- replay target/conformance/run-EXAMPLE/CONTRACT/minimal.json`. `--shrink-limit`, `--fuel`, `--memory` (bytes), and `--timeout` (seconds) bound reduction and execution. Proptest state machines generate complete valid start, host-release, completion, cancellation, observation, and reuse traces before compilation. The lifecycle adapter supplies controlled clocks to both Node and Wasmtime. These traces do not exhaustively explore asynchronous schedules. Ordinary Rust tests remain a separate CI gate.

### Compare component performance with main

Run the opt-in comparison in the pinned development environment:

```sh
nix develop -c node scripts/compare_components.mjs
```

The script archives local `main` (or an optional baseline ref argument), builds the same `tests/component_measurement.rs` harness against both compiler revisions in release mode, then prints a comparison table. It leaves the checkout and branch untouched. Run on an idle machine; both builds finish before measurements start.

Incoming-stream execution sums a 4 MiB input under a 64 KiB guest memory limit. Bounded HTTP reads a 4 MiB in-memory host response, with exact-limit success and one-byte overflow measured separately under a 16 MiB guest limit. Each workload uses a reused instance, five warmup calls, and five samples of five calls. Both revisions run in both placements on a current-thread Tokio runtime: directly awaited by its root future, and in an ordinary spawned task. Root placement can amplify repeated guest yields; the table keeps the two placements separate and compares matching placements. Timers exclude builds, guest and Wasmtime compilation, instantiation, input preparation, and outcome assertions. Checks verify results, resource cleanup, and no memory growth after warmup. HTTP timing includes P3 host transfers but no network. The two revisions use equivalent programs targeting their respective APIs; historical imports are confined to the baseline measurement source.

The original default-reader rows remain visible. Three additional BYOB rows compare direct reads into reusable caller buffers with the corresponding original API workload on the baseline. Both HTTP readers enforce the cap and check for overflow. An additional unbounded `response.arrayBuffer()` row reads the whole body without BYOB or a guest body-size cap. Its baseline reference is the original exact-limit read of the same body, which still enforces its cap; the overflow guarantees differ. One guest-level await can perform multiple internal stream reads. BYOB must match or beat the old API's execution medians under the same memory constraints.

The tables report median sample times, stripped component bytes, and committed guest linear memory after warmup (not process RSS), including size-only comparisons of each revision's `examples/merge_docs.ts`, `examples/merge_task.ts`, and `template/src/index.ts`. Raw samples, guest sources, component binaries, exact revisions, source fingerprints, toolchain identity, and the table are saved under `target/conformance/comparisons/run-*`. The existing text/filesystem measurements remain in the JSON report. Timing changes are local observations; repeat the command before interpreting small differences as improvements.

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
4. **SSA Lowering**: Lowers HIR into typed SSA basic blocks. Heap references are tracked in root frames across async suspension points, and algorithmic tasks (such as JSON codecs and date arithmetic) are statically linked from embedded, allocator-free Rust helpers.
5. **Code Generation**: WAFFLE optimizes SSA control flow graphs, enforces reducibility, and reconstructs structured core WebAssembly instructions.
6. **Component Encoding**: Synthesizes canonical ABI import and export adapters, embeds resolved WIT interface metadata via `wit_component::ComponentEncoder`, and emits the validated WASI 0.3 component.

For memory layouts, coroutine scheduling, and helper linking details, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Semantics

Perry-WIT supports a predictable, strictly typed subset of TypeScript designed for high-performance ahead-of-time compilation.

### Types
- **Shapes**: Declared interfaces/records, typed dictionaries, dense homogeneous arrays, finite unions, and validated JSON trees. Literal record field names are preserved even when matching builtins (`fetch`, `process`, `console`).
- **Integers**: Both `s64` and `u64` map to TypeScript `bigint` (including nested records, lists, options, and tuples). `BigInt(value)` construction supports finite safe integers; arbitrary-precision arithmetic and bigint literals are not implemented.
- **Strings**: Lower directly using their UTF-8 pointer and byte length without intermediate re-encoding.
- **Bytes**: Outbound WIT `list<u8>` accepts `string | Uint8Array` recursively across import arguments and exported results (records, options, variants, lists). The generated SDK provides `WitInput<T>` and named `TInput` aliases for outbound values. Incoming byte lists remain mutable `Uint8Array` values.
- **Dates**: `Date` supports immutable UTC operations, while supported `Temporal` APIs provide explicit ISO timestamp and calendar calculations.
- **Coercion**: Arbitrary runtime coercion, dynamic property deletion (`delete obj.prop`), and untyped prototype manipulation are rejected at compile time.

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
Host resources—such as open files, HTTP connections, and stream buffers—are managed directly by the host runtime. Perry-WIT preserves these resources across asynchronous suspensions and frees them automatically when an operation finishes or when an `AbortSignal` triggers cancellation.

Opaque interface resources have nominal SDK types. An imported resource exposes `drop*`; an exported resource additionally exposes `new*` and `*Rep`. Transferring or disposing ownership invalidates all aliases. Borrowed handles expire when the exporting call returns and cannot be disposed or transferred as owned handles. Dispose owned imports explicitly, typically in `finally`.

If an unhandled trap or abnormal exit occurs, the host reclaims all active resources cleanly during store disposal.

## Contributing

Human + AIs are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

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
