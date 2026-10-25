# perry-wit

**Perry-WIT** compiles a static subset of TypeScript ahead-of-time into WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and typed SSA MIR built on [pliron](https://github.com/pliron-org/pliron) directly to native WebAssembly instructions, linking only the `#![no_std]` runtime and standard-library functions a program reaches.
An integer `add` export compiles to 100 bytes, and a string `trim` export stays under 6 KiB.

- **Dual-mode authoring**: Write CLI commands with top-level `await`, export typed functions or HTTP handlers, or do both in one module, whose top-level code then initializes state once before any export runs.
- **Node-compatible**: Modules without WIT imports also run under Node.js, and property-based differential contracts (`perry-conformance`) compare their behavior in Node.js and Wasmtime.
- **WIT-refined numbers**: Specialize `u8`–`s64` parameters and in-range arithmetic to native `i32` and `i64` instructions via forward range and backward bitwidth analysis while preserving IEEE 754 `f64` semantics for unconstrained `number` values.
- **Tiered memory reclamation**: Persist module-level state across calls while reclaiming temporaries through SROA, bounded activation frames, and per-call task regions or precise safe-point collection.
- **Callback-driven concurrency**: Lower `async` functions to stackless state machines so `await`, streams, and Promise combinators suspend directly on WASI 0.3 callbacks.
- **Toolchain-free embedding**: Compile from in-memory buffers via `perry_wit::api` or the standalone WASI 0.2 compiler component (`perry-wit.wasm`) without an external Rust or LLVM toolchain.

[Homepage](https://ahirner.github.io/perry-wit/) · [Handbook](https://ahirner.github.io/perry-wit/docs/) · [Playground](https://ahirner.github.io/perry-wit/play/) · [Rust API](https://ahirner.github.io/perry-wit/rust/perry_wit/api/index.html) · [Architecture](ARCHITECTURE.md)

## Quickstart

Save a TypeScript entrypoint as `main.ts`:

```ts check=quickstart
export function greet(name: string): string {
  return "Hello, " + name;
}
```

```sh
perry-wit gen-types
perry-wit main.ts
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'greet("World")' dist/main.wasm
```

Compilation discovers `./wit` when present and otherwise derives the contract in memory. Top-level scripts become CLI commands; modules expose their named function exports. `number` maps to `f64`, and `bigint` maps to `s64` with range checks at the boundary.

To save a contract for editing, generate it while compiling:

```sh
perry-wit main.ts --gen-wit --wit interfaces --world app
# Edit interfaces/world.wit, then compile against it:
perry-wit main.ts --wit interfaces --world app
```

`--gen-wit` defaults to `wit/world.wit` and world `app`. The Wasm output remains controlled by `-o` and defaults to `dist/<stem>.wasm`.

The handbook's Authoring chapter covers commands, HTTP handlers and the project template.

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
nix flake check   # rustfmt (both workspaces), WASI WIT, CLI, Wasm compiler, site, SDK sync, template and example runs
```

Benchmark sizes exclude custom sections such as debug names. Initialization fuel includes explicit data copying, with Wasmtime's platform-specific copy-on-write initialization disabled. When a change moves benchmark numbers on purpose, run the benchmark with `PERRY_EXPECT_BENCH=1` and commit the rewritten `tests/benchmark.json` together with the change.
CI additionally runs the collectible runtime tests (`--all-features`), Clippy for both workspaces and all conformance contracts.

### Flake outputs

```sh
nix build .#perry-wit           # compiler CLI
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
