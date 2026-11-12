# Embedding

The compiler ships as the `perry_wit` Rust crate and as a standalone WASI 0.2 component.
The runtime and standard libraries are precompiled and embedded in the compiler binary, so compilation runs entirely in memory.

## Rust

[`perry_wit::api::compile`](../rust/perry_wit/api/fn.compile.html) compiles an entry module, auxiliary ESM modules and WIT packages from byte buffers:

```rust
{{#include embedding-example.rs}}
```

Errors are [`CompileError`](../rust/perry_wit/struct.CompileError.html) values holding [`PerryDiagnostic`](../rust/perry_wit/struct.PerryDiagnostic.html) records with a source span and a [`DiagnosticCode`](../rust/perry_wit/enum.DiagnosticCode.html).

## JavaScript and TypeScript

The `perry-wit` npm package compiles TypeScript into components from Node.js 22 or later and from browsers.
It bundles the compiler, so installing it needs no Rust toolchain:

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
- `generateTypes({ wit, world })` returns the declaration files that `perry-wit gen-types` writes, as `{ path, contents }` records.

Each function takes its inputs from memory:

- `source` is the entry module's text or UTF-8 bytes.
  Pass `{ path, contents }` instead to name it in diagnostics and to resolve its relative imports.
- `modules` maps package-relative paths, such as `src/math.ts`, to the modules the entry module imports.
  It may be an object or a `Map`.
- `wit` is the application's WIT package: one file's text, an array of files that declare one package, or a binary-encoded package.
- `witDependencies` lists the packages the application package uses.
  WASI 0.3 packages are built in.
- `world` selects a world when the package declares more than one.

A rejected program throws a `CompileError`.
Its `diagnostics` hold the same codes, spans, notes and help as the Rust API.
Its `message` formats them as `file:line:column: severity[code]: message` lines.

The functions are asynchronous because the compiler loads on first use.
`await Compiler.load()` returns a compiler whose `compile`, `compileCore` and `generateTypes` methods run synchronously, for example inside a bundler's synchronous transform hook.

The compiler gets no directories, arguments or environment variables.
Node.js reads the compiler from the installed package.
Browsers fetch it through static `new URL(…, import.meta.url)` references, which bundlers such as Vite copy into their output.

### Building the Package

The package lives in the pnpm workspace under `js/`.
Its build has three steps:

1. `build:wasm` builds the [WebAssembly component](#webassembly-component) below with Cargo.
2. `build:bindings` generates JavaScript bindings for it with `jco transpile` from [Jco](https://github.com/bytecodealliance/jco).
3. `build:ts` compiles the TypeScript API that wraps the bindings.

```sh
cd js
pnpm install
pnpm build
pnpm test
pnpm test:e2e
```

`build:wasm` needs the development shell's `WASI_WIT_PATH`.
Setting `PERRY_COMPILER_WASM` to an already built compiler component skips Cargo:

```sh
PERRY_COMPILER_WASM="$(nix build .#perry-wit-wasm --print-out-paths)/lib/perry-wit.wasm" pnpm --dir js build
# for a debug build:
cargo build --target wasm32-wasip2 --config 'profile.dev.strip="debuginfo"'
PERRY_COMPILER_WASM=target/wasm32-wasip2/debug/perry-wit.wasm pnpm --dir js build
```

`pnpm test:e2e` tests the package as users receive it:

1. It packs the package, which runs the complete build.
2. It installs the tarball into a fresh copy of the project in `js/e2e/consumer`.
3. It checks which files the package ships.
4. It type-checks the project with Node.js and bundler module resolution, then runs it.

`js/pnpm-workspace.yaml` sets the supply-chain policy for every install, including the end-to-end test's:

- `minimumReleaseAge` installs only versions published at least a week ago.
- `blockExoticSubdeps` refuses transitive dependencies from git repositories or tarball URLs.
- `trustPolicy: no-downgrade` refuses a version published with weaker provenance than an earlier one.
- `strictDepBuilds` fails the install when a dependency's build script is not approved in `allowBuilds`.

The `JS package` workflow builds, tests and end-to-end tests the package on the oldest and newest supported Node.js releases.

## WebAssembly Component

[`perry-wit.wasm`](../downloads/perry-wit.wasm) is a WASI 0.2 component that exports this interface ([`compiler.wit`](../downloads/compiler.wit)) next to a `wasi:cli/run` command:

```wit
{{#include ../../wit/compiler/world.wit}}
```

The [playground](../play/index.html) runs it in a Web Worker.
Because the `api` export operates on in-memory buffers, a server can instantiate it with empty filesystem preopens, fuel limits and memory caps to compile untrusted TypeScript.
