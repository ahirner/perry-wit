# WASI Preview 2 TypeScript Component Template

This template provides a zero-config developer experience for authoring hermetic WebAssembly components in TypeScript using [Perry-WIT](https://github.com/<ORG-TBD>/perry-wit).

## Quickstart

1. **Enter the development shell:**
   ```bash
   nix develop
   ```
   Entering the Nix devshell automatically detects `wit/world.wit`, generates TypeScript declarations in `.perry/types/world.d.ts`, and configures `tsconfig.json`.

2. **Type check with `tsc`:**
   ```bash
   tsc --noEmit
   ```

3. **Build the component hermetically:**
   ```bash
   nix build
   ```
   Or compile directly with `perry-wit`:
   ```bash
   perry-wit src/index.ts --wit wit --world task -o dist/my_task.wasm
   ```

4. **Execute directly with wasmtime:**
   ```bash
   wasmtime run --invoke 'run-task("hello-world")' result/lib/my-task.wasm
   ```
