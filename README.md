# perry-wit

`perry-wit` compiles TypeScript ahead-of-time directly into native WebAssembly (WASI Preview 2) components. Rather than bundling dynamic JavaScript interpreters (such as QuickJS or SpiderMonkey), it lowers TypeScript through Perry's compiler pipeline and fuses the resulting module with an in-process static linker.

For runtime boundaries, compilation pipeline details, and conformance specifications, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Environment

For developing the Perry-WIT compiler, enter the repository's default shell:

```bash
nix develop
```

This supplies the Rust toolchain, Node.js, TypeScript, Wasmtime, `wasm-tools`, and
WASI Preview 2 definitions via `$WASI_WIT_PATH` and WASI 0.3 definitions via
`$WASI_P3_WIT_PATH`. The latter are also available through `nix build .#wasi-p3-wit`;
the `wasi-p3-wit` flake check resolves their complete CLI and HTTP dependency graph.
Cargo uses the prepared guest
runtime through `$GUEST_RUNTIME_PATH`; use `cargo build`, `cargo run`, and
`cargo test` to work on the compiler.

For authoring TypeScript components, select the SDK shell:

```bash
nix develop .#sdk
```

The SDK shell provides the packaged `perry-wit` compiler, `tsc`, Wasmtime, and
`wasm-tools`, and generates contracts for the current project's `wit/` directory.
See [Authoring Components](#authoring-components) for the template workflow.

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

From the compiler checkout, compile a TypeScript script to a WASI Preview 2 component:

```bash
cargo run -- examples/merge_docs.ts -o dist/my_component.wasm
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

From an empty component project directory, initialize the template using a
Perry-WIT flake reference. For example, with a compiler checkout:

```bash
perry_wit_source="git+file:///absolute/path/to/perry-wit"
nix flake init -t "$perry_wit_source"
```

Set `inputs.perry-wit.url` in the generated `flake.nix` to the same reference,
then run `nix develop`. A repository reference such as
`github:<ORG-TBD>/perry-wit` can also be used; substitute the chosen organization.
The template's `../` default resolves relative to its directory.
To override the configured source for a command, pass
`--override-input perry-wit "$perry_wit_source"` to `nix develop` or `nix build`.

The template's default shell selects Perry-WIT's `devShells.sdk`, so plain
`nix develop` enters the component-author environment in generated projects.

For an existing component project, enter the SDK shell directly from its directory:

```bash
nix develop "$perry_wit_source#sdk"
```

When entering the SDK shell:

- `perry-wit gen-types` runs automatically if a `wit/` directory is present, emitting `.perry/types/world.d.ts` and `.perry/types/implementation-check.ts`.
- A missing `tsconfig.json` is generated with the implementation check included. The template already includes these files in its configuration.
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

Generation also writes `.perry/types/implementation-check.ts`, which binds the
WIT contract to `src/index.ts`. Select another implementation with
`--entry src/my-task.ts`. The generated default tsconfig includes this check;
an existing custom tsconfig must include it in its `files` or `include` list.
Interface members use prefixed implementation names, such as `apiRunTask` for
`api`'s `run-task`; `ComponentImplementation` in `world.d.ts` lists the exact names.

## Contributing

Develop inside `nix develop` to ensure matching toolchain versions across dependencies. Format all code with `cargo fmt --all` and ensure both `cargo test` and `nix flake check` pass cleanly before submitting changes.


Task export ABI support currently covers strings, numeric and boolean scalars,
void results, and `result<string, string>` results. WIT record parameters and
results are rejected with a compile-time diagnostic until canonical record
marshalling is implemented. Records use canonical field layouts at a component
boundary; JSON payloads should be declared as WIT `string` and parsed explicitly.
The SDK can describe composite WIT types beyond the compiler's current ABI support.

`fetch` supports GET and POST, string request bodies, and headers supplied as a
plain object or an array of name/value pairs. Other request options fail with a
diagnostic before sending the request. HTTP error statuses remain readable
responses with `status` and `ok` properties.
