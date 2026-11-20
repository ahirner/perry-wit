# Embedding

The compiler ships as the `perry_wit` Rust crate, the `perry-wit` npm package and a standalone WASI 0.2 component.
The runtime and standard libraries are precompiled and embedded in the compiler binary, so compilation runs entirely in memory.

## Rust

[`perry_wit::api::compile`](../rust/perry_wit/api/fn.compile.html) compiles an entry module, auxiliary ESM modules and WIT packages from byte buffers:

```rust
{{#include embedding-example.rs}}
```

## JavaScript API

perry-wit compiles a statically typed subset of TypeScript into WebAssembly components.
The npm package bundles the compiler for use in browsers and in Node.js 22 or later.

```sh
npm install perry-wit  # todo: publishing still to come
```

This script compiles a module against a WIT world and writes the component:

```ts
{{#include ../../js/packages/perry-wit/examples/compile.ts:compile}}
```

The package exports three functions:

- `compile(options)` returns the `component` bytes, the `core` module inside it and an `allocationReport`.
  - `coreOnly: true` returns only `core` and `allocationReport`, like `--core-only`.
  - `keepNames: true` keeps function names in the component, like `--keep-names`.
- `compileCore({ source, modules })` compiles raw TypeScript exports into a core module without a WIT world.
- `generateTypes({ wit, witDependencies, world })` returns the declaration files that `perry-wit gen-types` writes, as `{ path, contents }` records.

Each function takes its inputs from memory:

- `source` is the entry module's text or UTF-8 bytes.
  Pass `{ path, contents }` instead to name it in diagnostics and to resolve its relative imports.
- `modules` maps package-relative paths, such as `src/math.ts`, to the modules the entry module imports.
  It may be an object or a `Map`.
- `wit` is the application's WIT package: one file's text, an array of files that declare one package, or a binary-encoded package.
- `witDependencies` lists the packages the application package uses.
  WASI 0.3 packages are built in.
- `world` selects a world when the package declares more than one.

The functions are asynchronous because the compiler loads on first use.
`await Compiler.load()` returns a compiler whose `compile`, `compileCore` and `generateTypes` methods run synchronously, for example inside a bundler's synchronous transform hook.

The WASI host isolates the compiler with empty preopens, arguments and environment.
Node.js reads the compiler from the installed package.
Browsers fetch it through static `new URL(…, import.meta.url)` references, which bundlers such as Vite copy into their output.

## WebAssembly Component

The WASI 0.2 [compiler component](../downloads/perry-wit.wasm) exports `perry:compiler/api` for in-memory compilation and `wasi:cli/run` for command-line use.
Generate bindings for this package from [`compiler.wit`](../downloads/compiler.wit).

```wit
{{#include ../../wit/compiler/world.wit:1}}
```

The [playground](../play/index.html) runs it in a Web Worker.
Because the `api` export operates on in-memory buffers, a server can instantiate it with empty filesystem preopens, fuel limits and memory caps to compile untrusted TypeScript.

## Diagnostics

On failure, Rust returns a [`CompileError`](../rust/perry_wit/struct.CompileError.html), JavaScript throws a `CompileError`, and the WIT functions return a list of diagnostics.
Display the Rust error or JavaScript `error.message` to show the diagnostic with any source locations, notes, and help.

Match `error.primary().kind()` in Rust or `error.primary.kind` in JavaScript to handle an [error category](../rust/perry_wit/enum.ErrorKind.html).
The stable `code` identifies the specific error. New codes can appear within a category.

Source spans use one-based lines and columns. Columns count Unicode scalar values.
For editors that use UTF-16 columns, convert from the span’s zero-based UTF-8 byte offsets.
