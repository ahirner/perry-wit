# Embedding

Perry-WIT is architected as an embeddable, deterministic library with no runtime dependency on external system toolchains, LLVM, or Cargo. Hosts can invoke the compiler directly from Rust or execute the self-hosted WebAssembly compiler component inside any WASI-capable runtime.

## Rust Compiler API

The [`perry_wit::api`](../rust/perry_wit/api/index.html) module compiles in-memory TypeScript source modules and WIT package definitions directly into validated WASI 0.3 WebAssembly component bytes:

```rust
use perry_wit::api::{self, Options, Program, Wit};

let program = Program {
    source: br#"
        export function authorize(token: string): boolean {
            return token.startsWith("Bearer ") && token.length > 16;
        }
    "#
    .as_slice()
    .into(),
    modules: vec![],
};

let wit = Wit {
    application: br#"
        package envoy:auth@0.1.0;
        world filter {
            export authorize: func(token: string) -> bool;
        }
    "#
    .as_slice()
    .into(),
    dependencies: vec![],
};

let compiled = api::compile(&program, &wit, &Options::default())?;
assert!(!compiled.bytes.is_empty());
```

- **Zero Filesystem Side Effects**: `api::compile` resolves relative ESM imports from `Program::modules` and WIT package dependencies from `Wit::dependencies` entirely in memory.
- **Embedded Guest Helpers**: Freestanding Rust guest helpers (JSON, UTF-8, calendar arithmetic, URL parsing) are pre-compiled to `wasm32-unknown-unknown` at compiler build time and embedded into the `perry_wit` crate binary.
- **Structured Diagnostics**: Compilation errors return structured source spans, offending AST constructs, and WIT contract mismatch descriptions.

## Self-Hosted WebAssembly Component

Perry-WIT itself compiles to a standalone WASI WebAssembly component (`perry-wit-compiler.wasm`) implementing the `perry:compiler/api` WIT world:

```wit
package perry:compiler@0.1.0;

interface api {
  record module-file {
    path: string,
    contents: list<u8>,
  }
  record compile-request {
    source: list<u8>,
    modules: list<module-file>,
    wit-application: list<u8>,
    wit-dependencies: list<module-file>,
    world-name: option<string>,
    core-only: bool,
    keep-names: bool,
  }
  compile: func(req: compile-request) -> result<list<u8>, string>;
}

world compiler {
  export api;
}
```

This self-contained compiler artifact enables:

- **Browser Compilation**: The [interactive playground](../play/index.html) runs `perry-wit-compiler.wasm` entirely client-side inside the browser to compile TypeScript and WIT edits into runnable WASI 0.3 components in milliseconds.
- **Sandboxed Multi-Tenant Control Planes**: Edge gateways and serverless platforms can compile tenant-submitted TypeScript plugins inside a memory- and fuel-limited Wasm sandbox before deploying the resulting component.

## Resource Budgets

Compiled components adhere to strict runtime budgets enforced in CI:

| Metric | Guarantee | Verification |
| :--- | :--- | :--- |
| **Baseline Linear Memory** | `64 KiB` (1 Wasm page) for minimal components | Enforced by component budget tests across scalar and string exports. |
| **Sustained Allocation Churn** | Zero unbounded heap growth across serial calls | `cabi_post_<export>` and loop-level mark-sweep reclaim temporary graphs deterministically. |
| **Fuel Scaling** | Linear $O(n)$ instruction scaling ($n \to 4n \le 6\times$ fuel) | Verified by differential scaling ratchets in `perry-conformance`. |
| **Binary Footprint** | Capability-pruned helper relocation | Unreferenced guest helpers and static data segments are stripped during codegen. |
