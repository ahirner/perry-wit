# WASI 0.3 TypeScript Component Template

This template provides a development environment for authoring hermetic WebAssembly components in TypeScript using Perry-WIT.

Configure `inputs.perry-wit.url` in `flake.nix` with the compiler's flake reference.
Use an immutable revision, for example `github:<organization>/perry-wit/<full-commit>`.
The template inherits the compiler’s pinned toolchain and dependencies; it has no
independent nixpkgs or flake-utils input. Authors need not commit a consumer
`flake.lock` or generated `.perry` files. The compiler maintains its own locks.
A local `git+file:///absolute/path/to/perry-wit` reference is useful during compiler development.
The template’s `../` placeholder must be replaced when creating a consumer project.
To override the configured source for a command, pass
`--override-input perry-wit git+file:///absolute/path/to/perry-wit` to
`nix develop` or `nix build` instead.

## Quickstart

1. **Enter the component SDK shell:**
   ```bash
   nix develop --no-write-lock-file
   ```
   This template maps its default shell to Perry-WIT's `mkSdkShell` with the same world and entry as the build. It provides
   Node, `perry-wit`, `tsc`, Wasmtime, and `wasm-tools`. On entry it generates declarations
   in `.perry/types/world.d.ts` and an implementation check in
   `.perry/types/implementation-check.ts`; the supplied `tsconfig.json` includes both.

   Existing projects can use `lib.<system>.mkSdkShell` with their own `wit`,
   `world`, and `entry` configuration, as shown in this template's `flake.nix`.
   Inside the compiler checkout, `nix develop .#sdk` checks the merge-task example;
   plain `nix develop` selects the Rust contributor environment.

2. **Type check with `tsc`:**
   ```bash
   tsc --noEmit -p .perry/types
   ```

3. **Build the component hermetically:**
   ```bash
   nix build --no-write-lock-file
   ```
   Or compile directly with `perry-wit`:
   ```bash
   perry-wit src/index.ts --wit wit --world task -o dist/my_task.wasm
   ```

4. **Execute directly with wasmtime:**
   ```bash
   wasmtime run -S p3=y -W component-model-async=y --invoke 'run-task("hello-world")' result/lib/my-task.wasm
   ```
