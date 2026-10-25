# TypeScript Component Template

Set `inputs.perry-wit.url` in `flake.nix` to a pinned compiler revision, such as `github:ahirner/perry-wit/<commit>`.

```sh
nix develop --no-write-lock-file
tsc --noEmit -p .perry/types
nix build --no-write-lock-file
wasmtime run -S p3=y -W component-model-async=y --invoke 'run-task("hello-world")' result/lib/my-task.wasm
```

The SDK shell provides the compiler, Node.js, TypeScript, Wasmtime, and `wasm-tools`. It generates runtime declarations for the editor. `nix build` type-checks `src/index.ts` and compiles its exported function using an inferred contract.

Compile directly with:

```sh
perry-wit src/index.ts -o dist/my-task.wasm
```

To create an editable contract, run `perry-wit src/index.ts --gen-wit`. Edit `wit/world.wit`, then run `perry-wit gen-types` and compile again. Ordinary compilation discovers the authored `wit` directory automatically.
