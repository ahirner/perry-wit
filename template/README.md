# WASI Preview 2 TypeScript Component Template

This template provides a zero-config developer experience for authoring hermetic WebAssembly components in TypeScript using [Perry-WIT](https://github.com/<ORG-TBD>/perry-wit).

The compiler input resolves from the published repository. To test a local compiler checkout, use `nix develop --override-input perry-wit path:/absolute/path/to/perry-wit` or the same override with `nix build`.

## Quickstart

1. **Enter the component SDK shell:**
   ```bash
   nix develop
   ```
   This template maps its default shell to Perry-WIT's `devShells.sdk`. It provides
   `perry-wit`, `tsc`, Wasmtime, and `wasm-tools`. On entry it generates declarations
   in `.perry/types/world.d.ts` and an implementation check in
   `.perry/types/implementation-check.ts`; the supplied `tsconfig.json` includes both.

   To use the SDK in an existing component project without this template, run
   `nix develop github:<ORG-TBD>/perry-wit#sdk` from that project's directory.
   Inside a Perry-WIT compiler checkout, `nix develop .#sdk` selects this shell;
   plain `nix develop` there selects the Rust contributor environment.

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
   wasmtime run -S http=y -S inherit-network=y --invoke 'run-task("hello-world")' result/lib/my-task.wasm
   ```
