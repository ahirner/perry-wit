# TypeScript Component Template

Set `inputs.perry-wit.url` in `flake.nix` to a pinned compiler revision, such as `github:ahirner/perry-wit/<commit>`.

```sh
nix develop --no-write-lock-file
tsc --noEmit -p .perry/types
nix build --no-write-lock-file
wasmtime run -S p3=y -W component-model-async=y --invoke 'run-task("hello-world")' result/lib/my-task.wasm
```

The SDK shell provides `perry-wit`, Node.js, `tsc`, Wasmtime and `wasm-tools`, and generates `.perry/types` on entry. `nix build` type-checks `src/index.ts` and compiles `result/lib/my-task.wasm`.

Inside the shell, compile directly with `perry-wit src/index.ts -o dist/my-task.wasm`, or pass `--gen-wit` to write `wit/world.wit` for customization (re-run `perry-wit gen-types` after editing it).
