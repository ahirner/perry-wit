# perry-wit

**Perry-WIT** compiles a static subset of TypeScript ahead of time into WebAssembly components for **WASI 0.3**.
It lowers TypeScript through [Perry](https://www.perryts.com) HIR and typed SSA MIR built on [pliron](https://github.com/pliron-org/pliron) directly to WebAssembly instructions, freeing each call's temporaries on return and suspending `await` through WASI 0.3 callbacks.

[Homepage](https://ahirner.github.io/perry-wit/) · [Handbook](https://ahirner.github.io/perry-wit/docs/) · [Playground](https://ahirner.github.io/perry-wit/play/) · [Rust API](https://ahirner.github.io/perry-wit/rust/perry_wit/api/index.html) · [Architecture](ARCHITECTURE.md)

## Quickstart

A WIT world declares the exports; `kebab-case` WIT names map to `camelCase` TypeScript functions:

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

The [Authoring](https://ahirner.github.io/perry-wit/docs/authoring.html) chapter covers commands, HTTP handlers and the project template.

## Development

Enter the pinned development shell with `direnv allow` or `nix develop`.
It provides Rust, Node.js, TypeScript, Wasmtime and `wasm-tools`, sets `WASI_WIT_PATH` to the WASI 0.3 packages, and shares helper builds through `PERRY_HELPER_TARGET_DIR`.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p perry-conformance -- check
nix flake check
```

`nix build .#perry-wit` builds the CLI, `nix build .#site` the homepage, handbook and playground, and `nix build .#example-merge-task` or `.#template-component` the reference components.

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” ([Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster)).

Register every new capability contract, input domain and boundary diagnostic in `crates/conformance`.
