# WASI Preview 2 TypeScript Component Template

This template provides a development environment for authoring hermetic WebAssembly components in TypeScript using Perry-WIT.

Configure `inputs.perry-wit.url` in `flake.nix` with the compiler's flake reference,
such as `git+file:///absolute/path/to/perry-wit` or
`github:<ORG-TBD>/perry-wit` with the chosen organization.
The default `../` resolves relative to the template directory.
To override the configured source for a command, pass
`--override-input perry-wit git+file:///absolute/path/to/perry-wit` to
`nix develop` or `nix build` instead.

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
   `nix develop "git+file:///absolute/path/to/perry-wit#sdk"` from that project's directory.
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
