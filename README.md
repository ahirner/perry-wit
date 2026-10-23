# perry-wit

**Perry-WIT** compiles static TypeScript ahead-of-time into standalone WebAssembly components targeting **WASI 0.3 (Preview 3)**.

 Perry-WIT lowers TypeScript through [Perry](https://www.perryts.com) HIR and typed SSA MIR built on [pliron](https://github.com/pliron-org/pliron) straight to native WebAssembly instructions. Compiled components launch instantly, run within a single `64 KiB` linear memory page for baseline workloads, and suspend asynchronously through native WASI 0.3 stackless coroutines.

- **Multi-Target Authoring**: Compile typed WIT library exports, WASI 0.3 HTTP service handlers, and standalone CLI commands supporting top-level `await`.
- **Stratified Numerical Tower**: Preserve IEEE 754 `f64` semantics for unconstrained numbers while specializing WIT-constrained paths to native `i32`/`i64` instructions and overflow-free `i64` intermediate registers.
- **Node.js Equivalence**: Share static ESM modules between Node.js test harnesses and native Wasm components, verified by an automated differential conformance ratchet.
- **Hermetic Toolchain**: Pin compilers, WASI 0.3 WIT definitions, SDK type generation, and browser verification through Nix flakes.

[Homepage](https://ahirner.github.io/perry-wit/) · [Handbook](https://ahirner.github.io/perry-wit/docs/) · [Browser Playground](https://ahirner.github.io/perry-wit/play/) · [Rust Embedding API](https://ahirner.github.io/perry-wit/rust/perry_wit/api/index.html) · [Architecture](ARCHITECTURE.md)

## Quickstart

Define a WIT world (`wit/world.wit`) and a matching TypeScript module (`src/index.ts`). WIT `kebab-case` exports map directly to `camelCase` functions in TypeScript:

```wit
package example:filter@0.1.0;

world auth-filter {
  export verify-token: func(header: string) -> result<string, string>;
}
```

```ts
export function verifyToken(header: string): string {
  if (!header.startsWith("Bearer ")) {
    throw new Error("missing bearer prefix");
  }
  return header.slice(7).trim();
}
```

Generate TypeScript declarations, compile to a WASI 0.3 component, and invoke under Wasmtime:

```sh
perry-wit gen-types --wit wit --world auth-filter --entry src/index.ts -o .perry/types
perry-wit src/index.ts --wit wit --world auth-filter -o dist/auth.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'verify-token("Bearer tok_live_98a")' dist/auth.wasm
```

To scaffold a new standalone project outside this repository:

```sh
nix flake init -t github:ahirner/perry-wit
```

## Embedding

The `perry_wit::api` module and self-hosted `perry:compiler/api` component compile in-memory TypeScript and WIT buffers directly to WebAssembly components:

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

## Documentation

The [Perry-WIT Handbook](docs/src/SUMMARY.md) covers the system in detail:

- **[Architecture](ARCHITECTURE.md)**: Seven-stage lowering pipeline, iterative `pliron` optimization loop, unified `talc` memory model, WASI 0.3 concurrency, and freestanding Rust guest helpers.
- **[Semantics](docs/src/semantics.md)**: Value representations, structured exception propagation, asynchronous byte streams, and the complete standard library inventory.
- **[Numerical Tower](docs/src/numerical-tower.md)**: Numeric representation stratification, the mixed-width subsumption join lattice, and `i64` register promotion.
- **[Authoring](docs/src/authoring.md)**: Library components, CLI commands, WASI 0.3 HTTP handlers, and WIT type generation.
- **[Embedding](docs/src/embedding.md)**: Rust `perry_wit::api` integration, the self-hosted WebAssembly compiler component, and runtime resource budgets.
- **[Conformance](docs/src/conformance.md)**: Differential Node.js oracle verification, linear fuel scaling ratchets, and the capability contract catalog.

## Development

Enter the pinned development shell via direnv or Nix:

```sh
direnv allow
# or
nix develop
```

Run the workspace verification suite before submitting changes:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo run -p perry-conformance -- check
nix flake check
```

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

Register every new capability contract, input domain, and boundary diagnostic in `crates/conformance`.
