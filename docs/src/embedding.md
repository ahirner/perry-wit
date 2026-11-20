# Embedding

The compiler ships as the `perry_wit` Rust crate, the `perry-wit` npm package and a standalone WASI 0.2 component.
The runtime and standard libraries are precompiled and embedded in the compiler binary, so compilation runs entirely in memory.

## Rust

[`perry_wit::api::compile`](../rust/perry_wit/api/fn.compile.html) compiles an entry module, auxiliary ESM modules and WIT packages from byte buffers:

```rust
{{#include embedding-example.rs}}
```

Errors are [`CompileError`](../rust/perry_wit/struct.CompileError.html) values holding [`Diagnostic`](../rust/perry_wit/struct.Diagnostic.html) records, each with an [`ErrorKind`](../rust/perry_wit/enum.ErrorKind.html), a stable [`Code`](../rust/perry_wit/struct.Code.html), optional [`SourceSpan`](../rust/perry_wit/struct.SourceSpan.html), optional spanned [`Note`](../rust/perry_wit/struct.Note.html) entries, and optional help text.


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

A rejected program throws a `CompileError` exposing `diagnostics` and `primary` (its first diagnostic).
Its diagnostics carry the same `kind`, kebab-case `code`, `span`, spanned `notes`, and `help` as the Rust API.
Its `message` (and `formatDiagnostics`) formats them identically to Rust `Display`:

```text
file:line:column: error[code]: message
  unspanned note
  file:line:column: spanned note
  help: suggestion
```

The functions are asynchronous because the compiler loads on first use.
`await Compiler.load()` returns a compiler whose `compile`, `compileCore` and `generateTypes` methods run synchronously, for example inside a bundler's synchronous transform hook.

The WASI host isolates the compiler with empty preopens, arguments and environment.
Node.js reads the compiler from the installed package.
Browsers fetch it through static `new URL(…, import.meta.url)` references, which bundlers such as Vite copy into their output.

## WebAssembly Component

[`perry-wit.wasm`](../downloads/perry-wit.wasm) is a WASI 0.2 component that exports this interface ([`compiler.wit`](../downloads/compiler.wit)) next to a `wasi:cli/run` command:

```wit
{{#include ../../wit/compiler/world.wit}}
```

The [playground](../play/index.html) runs it in a Web Worker.
Because the `api` export operates on in-memory buffers, a server can instantiate it with empty filesystem preopens, fuel limits and memory caps to compile untrusted TypeScript.

## Diagnostics

Every diagnostic pairs a broad, caller-actionable `ErrorKind` with a specific kebab-case `code`.

| Kind | Meaning |
| :--- | :--- |
| `invalid-input` | Malformed API input from the embedding caller: invalid UTF-8, invalid module path, duplicate module identity, or empty WIT sources. |
| `io` | A host filesystem read or write failed (native file entrypoints only). |
| `wit` | The WIT input is invalid on its own: syntax error, invalid package encoding, missing dependency, missing directory, or ambiguous world selection. |
| `syntax` | The TypeScript source does not parse. |
| `module` | The TypeScript module graph cannot be built: unresolved, cyclic, unknown, or ambiguous local import or export. |
| `type-check` | A TypeScript value does not fit its declared or required type: missing annotation, nullability, record fields, or type mismatch. |
| `unsupported` | Valid TypeScript, regex, or WIT construct that Perry-WIT does not compile yet. |
| `contract` | The TypeScript module's export or import surface disagrees with the WIT world: names, arity, async, error channels, or top-level statements. |
| `internal` | A compiler bug. |

### Classification rules

- A type or shape error on a TypeScript value is `type-check`, even when the expected type came from WIT.
- A declared TypeScript signature that disagrees with the WIT signature is `contract`; a TypeScript value that does not fit a declared type is `type-check`.
- A valid WIT type with no compiler ABI mapping is `unsupported`, not `wit`.
- A missing WIT dependency is `wit`; an unresolved TypeScript import is `module`.

### Versioning

Branch on `kind`, and use `code` for display, documentation, and tests.
Adding a variant to `ErrorKind` is a breaking change.
Adding a new diagnostic `code` is non-breaking; `code` crosses the WIT and JavaScript boundaries as a kebab-case `string`.

### Source spans

`startLine`, `endLine`, `startColumn`, and `endColumn` are 1-based, and columns count Unicode scalar values.
`startByte` and `endByte` are 0-based UTF-8 byte offsets.
Editors and LSP integrations that expect UTF-16 code-unit columns should derive them from `startByte` and `endByte`.
