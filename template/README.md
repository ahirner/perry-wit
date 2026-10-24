# WASI 0.3 TypeScript Component Template

Starter project for building WASI 0.3 WebAssembly components in TypeScript with Perry-WIT.

Set `inputs.perry-wit.url` in `flake.nix` to a pinned compiler flake reference (for example `github:ahirner/perry-wit/<commit>` or `git+file:///absolute/path/to/perry-wit`). The flake inherits the compiler's pinned toolchain and WASI 0.3 WIT definitions directly, keeping consumer repositories limited to `flake.nix`, `tsconfig.json`, `wit/`, and `src/`.

## Quickstart

1. **Enter the SDK shell:**
   ```bash
   nix develop --no-write-lock-file
   ```
   `mkSdkShell` provides Node.js, `perry-wit`, `tsc`, Wasmtime, and `wasm-tools`, and generates `.perry/types` for `wit/world.wit` and `src/index.ts` on entry.

2. **Type-check with `tsc`:**
   ```bash
   tsc --noEmit -p .perry/types
   ```

3. **Build the component:**
   ```bash
   nix build --no-write-lock-file
   ```
   Or compile directly with `perry-wit`:
   ```bash
   perry-wit src/index.ts --wit wit --world task -o dist/my_task.wasm
   ```

4. **Run with Wasmtime:**
   ```bash
   wasmtime run -S p3=y -W component-model-async=y --invoke 'run-task("hello-world")' result/lib/my-task.wasm
   ```
