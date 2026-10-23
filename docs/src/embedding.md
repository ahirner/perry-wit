# Embedding

The compiler ships as the `perry_wit` Rust crate and as a standalone WASI 0.2 component.
Prebuilt guest helper modules are embedded in the compiler binary, so compilation runs entirely in memory.

## Rust

[`perry_wit::api::compile`](../rust/perry_wit/api/fn.compile.html) compiles an entry module, auxiliary ESM modules and WIT packages from byte buffers:

```rust
{{#include embedding-example.rs}}
```

Errors are [`CompileError`](../rust/perry_wit/struct.CompileError.html) values holding [`PerryDiagnostic`](../rust/perry_wit/struct.PerryDiagnostic.html) records with a source span and a [`DiagnosticCode`](../rust/perry_wit/enum.DiagnosticCode.html).

## WebAssembly Component

[`downloads/perry-wit.wasm`](../downloads/perry-wit.wasm) is a WASI 0.2 component that exports this interface ([`compiler.wit`](../downloads/compiler.wit)) next to a `wasi:cli/run` command:

```wit
{{#include ../../wit/compiler/world.wit}}
```

The [playground](../play/index.html) runs it in a Web Worker.
Because the `api` export operates on in-memory buffers, a server can instantiate it with empty filesystem preopens, fuel limits and memory caps to compile untrusted TypeScript.
