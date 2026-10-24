# perry-wit

**Perry-WIT** compiles a static subset of TypeScript ahead-of-time into WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
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
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'verify-token("Bearer tok_live_98a")' dist/auth.wasm
```

The handbook's Authoring chapter covers commands, HTTP handlers and the project template.

## Development

Enter the pinned development shell with `direnv allow` or `nix develop`.
It provides Rust, Node.js, TypeScript, Wasmtime and `wasm-tools`, sets `WASI_WIT_PATH` to the WASI 0.3 packages, and shares runtime builds through `PERRY_RUNTIME_TARGET_DIR`.
Fetch the runtime workspace dependencies before the first compiler build; embedded runtime builds run offline.

```sh
cargo fetch --manifest-path runtime/Cargo.toml --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p perry-conformance -- check
nix flake check
```

### Flake outputs

```sh
nix build .#perry-wit           # compiler CLI
nix build .#site                # homepage, handbook and playground
nix build .#template-component  # starter template component
nix build .#example-merge-task  # reference component
```

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

Register every new capability contract, input domain and boundary diagnostic in `crates/conformance`.
