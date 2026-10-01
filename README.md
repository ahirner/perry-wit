# perry-wit

`perry-wit` compiles TypeScript ahead-of-time directly into native WebAssembly (WASI Preview 2) components. Rather than bundling dynamic JavaScript interpreters (such as QuickJS or SpiderMonkey), it lowers TypeScript through Perry's compiler pipeline and fuses the resulting module with an in-process static linker.

For runtime boundaries, compilation pipeline details, and conformance specifications, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Environment

A development environment is defined in `flake.nix`:

```bash
nix develop
```

This supplies `wasmtime`, `node`, `rustc` (with `wasm32-unknown-unknown`), `wasm-tools`, and dynamically resolved WASI Preview 2 WIT definitions via `$WASI_WIT_PATH`.

---

## Usage

### Build

Build components and tools using Nix:

```bash
# Build the perry-wit CLI
nix build .#perry-wit

# Build the guest runtime
nix build .#guest-runtime

# Build the example component
nix build .#example-merge-docs
```

Or build locally with Cargo:

```bash
cargo build --release
```

### Compile

Compile a TypeScript script to a WASI Preview 2 component:

```bash
perry-wit examples/merge_docs.ts -o dist/my_component.wasm
```

Pass `--core-only` to output unlinked Core WebAssembly without component wrapping, or `--wit <PATH>` and `--world <NAME>` to specify custom WIT contracts.

### Run

Execute the generated component with Wasmtime:

```bash
wasmtime run -S http=y -S inherit-network=y dist/my_component.wasm
```

### Test

Run unit tests, end-to-end integration tests, and flake validation:

```bash
# Unit and linker tests
cargo test

# End-to-end HTTP and splatting test
./scripts/test_e2e.sh

# Flake build and format checks
nix flake check
```

---

## Authoring

The authoring workflow for custom components provides instant type safety and tooling:

```
perry-wit/
├── flake.nix             # Toolchain & devShell definition
├── nix/
│   └── wasi.nix          # Pinned WASI Preview 2 WIT derivation
├── sdk/
│   ├── default.nix       # SDK packaging derivation
│   ├── package.json      # @perry/sdk npm package
│   ├── lib/
│   │   ├── generator.ts  # Generates .d.ts directly from world.wit
│   │   ├── shims.ts      # Node.js shims for local testing
│   │   └── harness.ts    # Dual-conformance test runner
│   └── templates/
│       ├── tsconfig.json # Base TypeScript configuration
│       └── task.ts       # Starter template
```

1. Define or import a `world.wit`.
2. Run `nix develop`:
   - Automatically parses `world.wit` and generates exact TypeScript definition files in `.perry/types/`.
   - Links `@perry/sdk` shims for local Node.js testing.
   - Provides `perry-wit`, `wasm-tools`, `wasmtime`, `nodejs`, and `tsc` directly in `$PATH`.
3. Run `tsc --noEmit` to validate types against the WIT contract.
4. Run `perry-wit <task.ts> -o dist/<task>.wasm` to compile the code to a verified WASIp2 component.
5. Builtin [examples](./examples) follow this contract.

## Contributing

Develop inside `nix develop` to ensure matching toolchain versions across dependencies. Format all code with `cargo fmt --all` and ensure both `cargo test` and `nix flake check` pass cleanly before submitting changes.
