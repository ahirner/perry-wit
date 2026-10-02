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

## Authoring Components

Create a new TypeScript WebAssembly component project from the template:

```bash
nix flake init -t github:<ORG-TBD>/perry-wit
```

Or explore the zero-config development shell in any component repository:

```bash
nix develop
```

When entering `nix develop`:
- `perry-wit gen-types` runs automatically if a `wit/` directory is present, emitting `.perry/types/world.d.ts`.
- `tsconfig.json` links `.perry/types/` for immediate IDE autocompletion and type safety.
- `perry-wit`, `tsc`, `wasmtime`, and `wasm-tools` are placed directly in `$PATH`.

### Type Checking & Building

Validate static types and build the component:

```bash
# Validate TypeScript implementation matches the WIT contract
tsc --noEmit

# Compile directly with the CLI
perry-wit src/index.ts --wit wit --world task -o dist/my_task.wasm

# Or package hermetically inside Nix via lib.buildComponent
nix build
```

You can also run type generation standalone:

```bash
perry-wit gen-types --wit wit --world task -o .perry/types
```

## Contributing

Develop inside `nix develop` to ensure matching toolchain versions across dependencies. Format all code with `cargo fmt --all` and ensure both `cargo test` and `nix flake check` pass cleanly before submitting changes.


Task export ABI support currently covers strings, numeric and boolean scalars,
void results, and `result<string, string>` results. WIT record parameters and
results are rejected with a compile-time diagnostic until canonical record
marshalling is implemented. Records use canonical field layouts at a component
boundary; JSON payloads should be declared as WIT `string` and parsed explicitly.
The SDK can describe composite WIT types beyond the compiler's current ABI support.
