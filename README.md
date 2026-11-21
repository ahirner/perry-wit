# perry-wit

**Perry-WIT** compiles a static subset of TypeScript ahead-of-time into WebAssembly components targeting **WASI 0.2 (Preview 2) and WASI 0.3 (Preview 3)**.

Rather than embedding a JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and typed SSA MIR built on [pliron](https://github.com/pliron-org/pliron) directly to native WebAssembly instructions, linking only the `#![no_std]` runtime and standard-library functions a program reaches.
An integer `add` export compiles to 100 bytes, and a string `trim` export stays under 6 KiB.

- **Dual-mode authoring**: Write CLI commands with top-level `await`, export typed functions or P3 HTTP handlers, or do both in one module, whose top-level code then initializes state once before any export runs.
- **Node-compatible**: Modules without WIT imports also run under Node.js, and property-based differential contracts (`perry-conformance`) compare their behavior in Node.js and Wasmtime.
- **WIT-refined numbers**: Specialize `u8`–`s64` parameters and in-range arithmetic to native `i32` and `i64` instructions via forward range and backward bitwidth analysis while preserving IEEE 754 `f64` semantics for unconstrained `number` values.
- **Tiered memory reclamation**: Persist module-level state across calls while reclaiming temporaries through SROA, bounded activation frames, and per-call task regions or precise safe-point collection.
- **Async on both targets**: Lower `async` functions to stackless state machines. P3 uses native component callbacks; P2 drives those state machines through WASI pollables inside synchronous exports, including concurrent outgoing HTTP requests.
- **Toolchain-free embedding**: Compile from in-memory buffers via `perry_wit::api` or the standalone WASI 0.2 compiler component (`perry-wit.wasm`), or from Node.js and browsers via the `perry-wit` npm package, without an external Rust or LLVM toolchain.

[Homepage](https://ahirner.github.io/perry-wit/) · [Handbook](https://ahirner.github.io/perry-wit/docs/) · [Playground](https://ahirner.github.io/perry-wit/play/) · [Rust API](https://ahirner.github.io/perry-wit/rust/perry_wit/api/index.html) · [Architecture](ARCHITECTURE.md)

## Quickstart

A WIT world declares the exports. `kebab-case` WIT names map to `camelCase` TypeScript functions:

```wit check=quickstart
package example:filter@0.1.0;

world auth-filter {
  export verify-token: func(header: string) -> result<string, string>;
}
```

```ts check=quickstart
export function verifyToken(header: string): string {
  if (!header.startsWith("Bearer ")) {
    throw new Error("missing bearer prefix");
  }
  return header.slice(7).trim();
}
```

```sh
perry-wit gen-types --wit wit --world auth-filter --entry src/index.ts -o .perry/types
perry-wit src/index.ts --wit wit --world auth-filter -o dist/auth.wasm
wasmtime run -C cache=n --invoke 'verify-token("Bearer tok_live_98a")' dist/auth.wasm
```

The handbook's Authoring chapter covers commands, HTTP handlers and the project template.

## P2 and P3 components

The selected WIT world determines the target. WASI `@0.2.x` interfaces select P2;
WASI `@0.3.x`, WIT `async` functions, futures or streams select P3. A world with
only synchronous custom interfaces also uses P2. Existing P3 worlds keep their
native asynchronous ABI.

A world may mix the two generations. Any WASI `@0.3.x` interface, `async`
function, future or stream makes the component a P3 component, and its WASI
`@0.2.x` interfaces are ordinary synchronous imports and exports that TypeScript
calls like any other WIT interface. In such a component:

- `process.env`, `Date.now()`, `performance.now()`, `Math.random()` and
  `crypto` use the WASI 0.3 interface when the world imports it, and the
  WASI 0.2 one otherwise.
- Services that wait (console output, timers, `fetch` and `node:fs`) need their
  WASI 0.3 interface. A P3 component cannot wait on WASI 0.2 pollables, so the
  compiler reports the interface to import.

For example, a P2 component can implement a synchronous WIT export with an async
TypeScript function:

```wit
package example:fetch;
world client {
  include wasi:cli/imports@0.2.12;
  import wasi:http/outgoing-handler@0.2.12;
  export sizes: func(first: string, second: string) -> u32;
}
```

```ts
async function size(url: string): Promise<number> {
  const response = await fetch(url);
  return (await response.bytes()).length;
}
export async function sizes(first: string, second: string): Promise<number> {
  const lengths = await Promise.all([size(first), size(second)]);
  return lengths[0] + lengths[1];
}
```

Compile with the usual `--wit` and `--world` options. The compiler embeds the
official WASI 0.2.12 WIT dependencies alongside its existing P3 definitions.
For another compatible P2 patch version, supply its WIT dependencies in your
project. P2 execution needs the host's P2 WASI and HTTP implementations and does
not require Component Model async support.

P2 supports commands, typed exports, timers, console output, environment,
randomness, filesystem promises and outgoing `fetch`. Include the interfaces
your APIs need; `wasi:cli/imports` supplies the standard CLI capabilities,
including `wasi:io/poll` for the guest event loop. HTTP responses resolve when
headers arrive and response bodies are read through P2 streams. Filesystem
promises use P2's synchronous descriptor operations. Exports remain synchronous
at the component boundary, so inter-component native async
calls, WIT futures/streams and the `wasi:http/handler@0.3.0` convenience export
require a P3 world. P2 does not provide P3 native task cancellation.

The [dual-preview example](examples/dual_preview/world.wit) compiles the same
TypeScript against either world:

```sh
perry-wit examples/dual_preview/index.ts --wit examples/dual_preview --world p2 -o dist/client-p2.wasm
perry-wit examples/dual_preview/index.ts --wit examples/dual_preview --world p3 -o dist/client-p3.wasm
```

## Development

Enter the pinned development shell with `direnv allow` or `nix develop`.
It provides Rust, Node.js, TypeScript, Wasmtime and `wasm-tools`, sets `WASI_WIT_PATH` to the WASI 0.3 packages, and shares runtime builds through `PERRY_RUNTIME_TARGET_DIR`.

```sh
cargo fetch --manifest-path runtime/Cargo.toml --locked    # once: runtime builds run offline
cargo test --workspace                                     # compiler tests; edited runtime modules rebuild
cargo test -p perry-mir                                    # one compiler crate, without runtime modules or Wasmtime
cargo test --test integration stream                       # integration tests matching a name
cargo test --manifest-path runtime/Cargo.toml --workspace  # runtime unit tests
cargo run -p perry-conformance -- check --select node.fs   # conformance contracts with an ID prefix
cargo test --test benchmark -- --ignored                   # sizes, fuel and memory against tests/benchmark.json
nix flake check   # rustfmt (both workspaces), WASI WIT, CLI, Wasm compiler, npm package, site, SDK sync, template and example runs
```

Benchmark sizes exclude custom sections such as debug names. Initialization fuel includes explicit data copying, with Wasmtime's platform-specific copy-on-write initialization disabled. When a change moves benchmark numbers on purpose, run the benchmark with `PERRY_EXPECT_BENCH=1` and commit the rewritten `tests/benchmark.json` together with the change.
CI additionally runs the collectible runtime tests (`--all-features`), Clippy for both workspaces and all conformance contracts.

### Flake outputs

```sh
nix build .#perry-wit           # compiler CLI
nix build .#npm-package         # perry-wit npm tarball
nix build .#site                # homepage, handbook and playground
nix build .#template-component  # starter template component
nix build .#example-merge-task  # reference component
```

### Upstream benchmarks

Compare Node and Wasmtime on Perry's official benchmark suite from the revision pinned in `Cargo.lock`.
`artifacts/bench-perry/` contains a correctness and performance table, peak memory measurements, and per-workload diagnostics.

```sh
nix run .#bench-perry
```

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

Register every new capability contract, input domain and boundary diagnostic in `perry-conformance`.
