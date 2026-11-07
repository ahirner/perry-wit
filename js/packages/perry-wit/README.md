# perry-wit

Compile TypeScript ahead of time into [WASI 0.3](https://wasi.dev) WebAssembly components, from Node.js or the browser.

[Perry-WIT](https://ahirner.github.io/perry-wit/) lowers a static subset of TypeScript directly to WebAssembly instructions instead of embedding a JavaScript engine.
This package bundles the compiler, so it needs no Rust or LLVM toolchain.

```sh
npm install perry-wit
```

```ts
import { compile, CompileError } from "perry-wit";

const { component } = await compile({
  source: `export function greet(name: string): string { return \`Hello, \${name}!\`; }`,
  wit: `package example:greeter@0.1.0;
        world greeter { export greet: func(name: string) -> string; }`,
});
```

- `compile(options)` compiles a program against a WIT world into a component.
- `compileCore(options)` compiles raw TypeScript exports into a core module.
- `generateTypes(options)` generates TypeScript declarations for a world's imports and exports.
- `Compiler.load()` loads a compiler whose methods run synchronously.

Invalid programs throw a `CompileError` holding the compiler's `diagnostics`.

The [Embedding](https://ahirner.github.io/perry-wit/docs/embedding.html) chapter of the handbook describes every option.
The [Authoring](https://ahirner.github.io/perry-wit/docs/authoring.html) chapter describes the TypeScript the compiler accepts.

## Development

The package builds from the [perry-wit repository](https://github.com/ahirner/perry-wit) inside its `nix develop` shell:

```sh
cd js
pnpm install
pnpm build   # cargo build, jco transpile, tsc
pnpm test
```
