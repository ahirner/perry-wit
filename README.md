# perry-wit

**Perry-WIT** compiles static TypeScript ahead-of-time directly into lightweight, high-performance WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a heavy JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and typed MIR built on [pliron](https://github.com/pliron-org/pliron) straight to native WebAssembly. This produces self-contained components with instant startup times, low linear memory usage, and zero runtime bloat.

- **Dual-mode authoring**: Write standalone CLI commands using top-level `await`, export typed functions matching a WIT interface, or combine both in a single source file.
- **Node-compatible**: Share code between Node.js test harnesses and native Wasm components using standard static ESM imports with matching type definitions.
- **Hermetic toolchain**: Fully reproducible builds, type generation, component testing via Nix flakes.

For runtime boundaries, memory layouts, compilation pipeline details, see [ARCHITECTURE.md](ARCHITECTURE.md).

[Homepage](https://ahirner.github.io/perry-wit/) · [Browser playground](https://ahirner.github.io/perry-wit/play/)

## Embedding

The Rust `api` module and `perry:compiler/api` WIT interface compile in-memory TypeScript and WIT buffers directly to WebAssembly components:

```rust
use perry_wit::api::{self, Options, Program, Wit};

let program = Program {
    source: b"export function greet(name: string): string { return 'Hello, ' + name; }".as_slice().into(),
    modules: vec![],
};
let wit = Wit {
    application: b"package example:greeting; world app { export greet: func(name: string) -> string; }".as_slice().into(),
    dependencies: vec![],
};
let compiled = api::compile(&program, &wit, &Options::default())?;
```

## Development

For compiler development, enter the pinned development shell with direnv or Nix:

```sh
direnv allow
# or
nix develop
```

This environment supplies pinned toolchains for Rust, Node.js, TypeScript, Wasmtime, `wasm-tools`.
Embedded helper builds share cached artifacts through `PERRY_HELPER_TARGET_DIR`.

- `WASI_WIT_PATH` identifies official WASI 0.3 packages.
- Application WIT dependencies in `wit/deps` automatically take precedence over ambient packages.

For component development outside this checkout, use [the starter template](template/README.md). Running `nix develop .#sdk` generates the TypeScript SDK for the target WIT world.

### Testing

Run the standard test suite with linters:

```sh
# Run unit and integration tests
cargo test --workspace

# Linting and formatting checks
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check

# Complete Nix flake checks
nix flake check
```

For formal capability verification against the Node.js oracle, see [Conformance](#conformance).

### Building

Build components with Cargo or Nix:

```sh
# Build the perry-wit compiler CLI with Cargo
cargo build --release

# Build compiler CLI with Nix
nix build .#perry-wit

# Build bundled example components with Nix
nix build .#example-merge-task
nix build .#template-component
```

Output binaries are placed in `target/release/perry-wit` or `result/bin/`.

## Authoring

Components can be authored as libraries exporting WIT functions, standalone CLI scripts, or complete projects based on the starter template.

### Library components

When implementing WIT interface exports, WIT defines function names, parameter types, return types, async effects.

Kebab-case WIT export names map automatically to camelCase in TypeScript. For example, `export run-task: func(input: string) -> string` in the WIT world maps to:

```ts
export function runTask(input: string): string {
  return "received: " + input;
}
```

Compile then invoke the library export:

```sh
perry-wit src/index.ts --wit wit --world task-runner -o dist/task.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("hello")' dist/task.wasm
```

- **Export Mapping**: WIT kebab-case export names map to TypeScript camelCase functions. Unexported functions remain internal.
- **Asynchronous Execution**: Exporting an `async func` expects a TypeScript function returning `Promise<T>`.
- **Type Marshalling**: Records, dense arrays, options, variants cross component boundaries directly. The 64-bit integers map to TypeScript `bigint`. Outbound byte lists accept UTF-8 strings or `Uint8Array` buffers.
- **Result Handling**: Explicit `{ ok, value }` objects, `{ ok: false, error }` objects, thrown errors map to WIT `result` variants.
- **Core Emission**: Passing `--core-only` emits the linked core module without component wrapping.

### Command components

A module with top-level statements compiles against a world exporting `wasi:cli/run@0.3.0`. Top-level statements, including top-level `await`, execute inside the generated runner:

```ts
const greeting = "Hello from WASI 0.3";
console.log(greeting);
```

Compile then run the command component:

```sh
perry-wit hello.ts --world command -o hello.wasm
wasmtime run -S p3=y -W component-model-async=y hello.wasm
```

- Standard streams write line-by-line using `console.log` and `console.error`.
- Setting `process.exitCode` or calling `process.exit(code)` terminates the command.
- Uncaught exceptions print diagnostic traces to stderr, returning an error exit code.
- Environment variables read via `process.env`. Clocks query platform providers via `Date.now()`.
- Console output drains through WASI P3 streams, requiring asynchronous exports.

### HTTP handlers

Modules implementing the HTTP handler world export `wasi:http/handler@0.3.0`, receiving incoming requests to produce HTTP responses:

```ts
export function handle(request: { method: string; pathWithQuery: string }): { status: number; headers: [string, Uint8Array][]; body: Uint8Array } {
  const body = new TextEncoder().encode("Hello from HTTP handler");
  return { status: 200, headers: [["content-type", new TextEncoder().encode("text/plain")]], body };
}
```

Generate types, compile, then serve:

```sh
perry-wit gen-types --wit wit/http-handler --world handler -o handler-types --no-tsconfig
perry-wit handler.ts --wit wit/http-handler --world handler -o handler.wasm
wasmtime serve -S p3=y -W component-model-async=y handler.wasm
```

- Each incoming request executes as an isolated guest task.
- Uncaught exceptions return an HTTP 500 internal error response, allowing the instance to continue serving subsequent requests.
- Request payloads stream directly through native WebAssembly buffers.

### Scaffolding

To start a new component project from the bundled template:

```sh
nix flake init -t git+file:///path/to/perry-wit
```

1. **Define WIT**: Specify the component world, interface dependencies.
2. **Generate Types**: Generate TypeScript definitions matching the WIT world:
   ```sh
   perry-wit gen-types --wit wit --world task --entry src/index.ts
   ```
3. **Type Check**: Validate implementation against the generated contract:
   ```sh
   tsc --noEmit -p .perry/types
   ```
4. **Compile Component**:
   ```sh
   perry-wit src/index.ts --wit wit --world task -o dist/task.wasm
   ```

## Conformance

Perry-WIT includes an executable conformance verification framework that differentially tests Perry-WIT against Node.js as the runtime oracle.

```sh
# Execute all capability contracts and directed tests
cargo run -p perry-conformance -- check

# List registered capability contracts
cargo run -p perry-conformance -- list

# Select specific capability groups
cargo run -p perry-conformance -- check --select node.fs
```

The conformance harness verifies capability contracts through an automated ratchet:
- Passing contracts prevent regressions. Any contract regression fails the test run.
- New contracts that pass must be registered, ensuring the verified surface only grows.
- Contracts outside the supported domain record documented deviations.
- Fuel scaling contracts enforce linear bounds on fuel consumption between proportional input sizes.

## Performance

Automated test suites evaluate component efficiency across four dimensions:

- **Resource Limits**: Component budget suites enforce strict binary size thresholds. Minimal components run within a single 64 KiB linear memory page.
- **Allocation Efficiency**: Integration suites verify bounded allocation churn, stream buffer reuse, zero memory growth across sustained workloads.
- **Execution Scaling**: Differential scaling tests verify linear fuel bounds ($n \to 4n \le 6\times$).
- **Profiling**: The `--keep-names` compiler flag preserves symbol names for guest profiling under Wasmtime. Release builds strip symbols by default.

## Compilation pipeline

Perry-WIT compiles TypeScript ahead-of-time directly into native WebAssembly instructions without an embedded JavaScript engine:

```text
TypeScript ──► Perry HIR ──► Typed MIR (pliron)
                                   │
                                   ▼
                           Optimization Loop
                                   │
                                   ▼
                         perry-codegen (pliron-wasm)
                                   │
                                   ▼
                       WASI 0.3 P3 Component
```

1. **HIR Lowering**: Transforms TypeScript AST into Perry HIR, resolving imports, establishing lexical scopes.
2. **MIR Lowering**: Lowers Perry HIR to typed pliron MIR with local variable slots, structured block joins, and root frames.
3. **Optimization**: Promotes local slots to SSA via `mem2reg`, iterating fact propagation, constant folding, and dead code elimination to convergence.
4. **Code Generation**: Translates optimized MIR to WebAssembly operations with operand stack scheduling and Ramsey dominator-tree control flow structuring.
5. **Component Encoding**: Relocates runtime memory management helpers, embeds WIT world metadata, and synthesizes Canonical ABI adapters.

## Semantics

Perry-WIT supports a strictly typed subset of TypeScript designed for ahead-of-time compilation:

- **Numbers**: Values evaluate as IEEE 754 64-bit floats by default, specializing to native integer representations when constrained by WIT boundaries or type annotations. See [Numerical Tower](#numerical-tower).
- **Records**: Supports interfaces, type literals, object literals with closed shapes. Structurally equivalent types share identical record layouts. Recursive interfaces are supported.
- **Arrays**: Arrays are dense, homogeneous. Out-of-bounds or non-integral indexing returns `undefined`. Writing at `length` appends a new element. Any other out-of-bounds write throws a `RangeError`.
- **Strings**: Text storage uses UTF-8 byte sequences with cached Unicode scalar length. Appending reuses spare buffer capacity for amortized linear growth. Indexing operates on Unicode scalar values, matching Node.js behavior for BMP scalars. Astral characters count as single scalar values, while surrogate halves are rejected.
- **Exceptions**: Standard error classes (`Error`, `TypeError`, `RangeError`, `ReferenceError`, and `SyntaxError`) are catchable via `try`, `catch`, `finally`, preserving `cause` properties across rethrows. Escaping exceptions map to declared WIT error variants or trap exports lacking an error mapping.
- **Control Flow**: Supports standard control flow constructs: conditional branches, loops, `break`, `continue`, short-circuit logic, ternary expressions, recursive function calls.
- **Async**: Asynchronous functions execute over the WASI 0.3 callback ABI. Awaiting an already-settled promise continues inline without microtask queue latency. Supported combinators include `Promise.all`, `Promise.race`, `Promise.allSettled`.
- **Byte Streams**: Supports default readers, asynchronous iteration, BYOB readers with caller-allocated buffers, minimum read lengths, explicit cancellation. Stream writes maintain bounded buffer reuse.
- **Temporal**: Supports ISO calendar operations via `Instant.from`, `PlainDate.from`, `PlainDateTime.from`. Instants expose `epochMilliseconds`. Plain dates and date-times accept numeric field objects with `overflow: "constrain" | "reject"`; omitted time fields default to zero. They expose date and time fields, ISO string conversion, and arithmetic via `.add({ days })`. Unsupported calendar formats, time zone manipulations produce compile-time diagnostics.
- **Standard Library**:
  - `Math`, `Uint8Array`, `TextEncoder`, `TextDecoder`, `crypto.getRandomValues`, `node:timers/promises`, `node:fs/promises`, HTTP `Headers`/`Request`/`Response`, standard stream bridges.
  - UTC `Date`, with `Date.parse` and `new Date(string)` accepting ISO date and date-time strings (malformed ISO strings return `NaN`; non-ISO legacy formats raise `RangeError`).
  - String manipulation and literal regular expressions, including named captures (`.groups` on `exec` and non-global `match`), named replacement strings, and synchronous replacement callbacks.
  - Schema-directed `JSON.parse` and `JSON.stringify`, including synchronous replacer callbacks and property-list arrays.

### Numerical Tower

Perry-WIT stratifies numeric representations across WebAssembly primitive types:

- **Floating-point default**: Unconstrained JavaScript `number` values evaluate as IEEE 754 64-bit floats (`f64`), preserving fractional division, remainder, negative zero, and `NaN` semantics.
- **Integer specialization**: Values constrained by WIT interfaces or explicit integer types (`u8`–`u64`, `s8`–`s64`) specialize to native WebAssembly integer instructions. WIT 64-bit integers (`s64`, `u64`) map to TypeScript `bigint`.
- **Bigint arithmetic**: Arbitrary-width `bigint` values support `+`, `-`, negation, comparison, decimal conversion, and range-checked WIT `s64`/`u64` boundary conversions; `*`, `/`, `%`, literals, and `BigInt(number)` operate on constant or fixed-width 64-bit values.
- **Subsumption joins**: Mixed numeric operations promote to the smallest representation subsuming both operands. Mixed signedness widens to the next enclosing signed width (e.g., `u8 + s8 → s16`, `u16 + s16 → s32`).
- **Optimization interaction**: Intermediate integer computations without narrow result requirements evaluate in native 64-bit `i64` registers, preventing premature overflow without heap allocations. Range inference and constant bounds analysis eliminate redundant bounds checks and modular reductions.

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

Run commands inside the direnv environment or `nix develop` to keep toolchains and build settings consistent.
Before submitting changes, ensure all formatting, lints, tests, flake checks pass:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p perry-conformance -- check
nix flake check
```

Register each claimed capability contract, input domain, directed boundary check in the conformance suite.
