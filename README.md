# perry-wit

`perry-wit` compiles TypeScript directly to native **WebAssembly WASI Preview 2** components using
[Perry](https://github.com/PerryTS/perry), a pure-Rust in-process module linker, and dynamic WIT contracts.

---

## Architecture

Unlike JS-on-Wasm runtimes that embed dynamic bytecode interpreters (QuickJS, SpiderMonkey, or Wasmi) at a cost of 1.4 MB to 15 MB, **perry-wit compiles TypeScript directly to WebAssembly bytecode**.
Compilation operates with **zero external binary dependencies**.

```
                       ┌────────────────────────────┐
                       │          input.ts          │
                       └─────────────┬──────────────┘
                                     │
                perry-parser / perry-hir / perry-codegen-wasm
                                     │
                                     ▼
                       ┌────────────────────────────┐
                       │   Core WebAssembly Module  │ (~10+ KB)
                       │   (Perry NaN-boxed ABI)    │
                       └─────────────┬──────────────┘
                                     │
                perry_wit::linker::merge_core_modules
                (pure-Rust static linker, remapping & resolving
                 runtime imports to shared linear memory)
                                     │
                                     ▼
       ┌──────────────────────────────────────────────────────────────┐
       │             WASI Preview 2 Component (Zero Interpreters)     │
       │                                                              │
       │   Canonical ABI entry: wasi:cli/run@x.y.z#run                │
       │   Execution: Native Cranelift JIT (No Wasmi / No QuickJS)    │
       │   Size: ~100 KB stripped                                     │
       │                                                              │
       │   Runtime Bridge:                                            │
       │     - Promise.all([fetch, fetch]) ──► concurrent wasi:http   │
       │     - Array destructuring         ──► NaN-box index access   │
       │     - Response.json()             ──► serde_json stream      │
       │     - Object splatting ({...a})   ──► native object_assign   │
       │     - console.log                 ──► wasi:cli/stdout        │
       │     - ...                         ──► wasi:...               │
       └──────────────────────────────┬───────────────────────────────┘
                                      │
                               wasmtime wasip2
                                      │
                                      ▼
                    WIT exports or output via WASIp2 stdout
```

### Components

1. **Host Compiler CLI (`perry-wit`)**:
   - `perry-parser`: Parses TypeScript source code into an AST.
   - `perry-hir`: Lowers AST to Perry High-Level Intermediate Representation.
   - `perry-codegen-wasm`: Compiles HIR into a Core WebAssembly binary with Perry's NaN-boxed ABI (`string_new`, `mem_call`, `fetch_with_options`, etc.).
   - Applies HIR rewrites for IIFEs, `NativeMethodCall`, etc.
   - Optional: Synthesizes `wasi:cli/run` entry point and aligns linear memory to 32 pages (2 MB).
2. **Rust In-Process Linker (`perry_wit::linker`)**:
   - Statically fuses the compiled TypeScript module (`env`) and the guest runtime module (`rt`) in memory.
   - Eliminates all C/C++ Binaryen (`wasm-merge`) dependencies.
   - Remaps function, type, and global indices, binds linear memory, and resolves all runtime function calls into direct internal calls.
3. **Rust Componentization & Stripping (`perry_wit::component`)**:
   - Embeds WIT interfaces and world declarations into the linked Core Wasm module using `wit-component`.
   - Dynamically resolves WASI Preview 2 WIT packages in topological order (`io` $\rightarrow$ `random` $\rightarrow$ `clocks` $\rightarrow$ `filesystem` $\rightarrow$ `sockets` $\rightarrow$ `cli` $\rightarrow$ `http`).
   - Strips debug and producer custom sections via `wasm-encoder`.
4. **Guest Runtime (`crates/guest-runtime`)**:
   - Minimal C-ABI runtime module targeting `wasm32-unknown-unknown` with imported linear memory.
   - Unified handle storage (`JsHandle`) avoiding ID collisions across JSON values, arrays, and in-flight HTTP streams.
   - Non-blocking `wasi:http/outgoing-handler` dispatch allowing multiple outgoing HTTP requests to run concurrently on the host event loop.
   - Strict WASI resource drop ordering satisfying Component Model lifetime invariants.

---

## Footprint

| Architecture | Approach | Size (Stripped) | Overhead / Runtime Engines |
| :--- | :--- | :--- | :--- |
| **Componentize-JS** | SpiderMonkey | ~5 MB – 15 MB | Heavy JS engine |
| **Javy** | QuickJS | ~1.5 MB – 2.0 MB | In-wasm JS interpreter |
| **perry-wit (if interpreted)** | Wasmi | ~1.4 MB | In-wasm WebAssembly interpreter |
| **perry-wit (current)** | **Native Ahead-of-Time** | **~100+ KB** | **Zero interpreters, direct native code** |

---

## Environment

A hermetic devShell is defined in `flake.nix`. Run:

```bash
nix develop
```

This supplies all required dependencies in your shell:
- `wasmtime >= 48.0` (48.0.1)
- `node` (24.x)
- `rustc` pinned with `wasm32-unknown-unknown` and `wasm32-wasip2` targets
- `wasm-tools`
- Official WASI Preview 2 WIT definitions dynamically sourced from flake input (`github:WebAssembly/WASI/v0.2.6`), exposed via `$WASI_WIT_PATH` without vendored files.

All build and test commands below can be executed directly within `nix develop` (or locally if the tools are already installed).

---

## Usage

### Single-Command Hermetic Build

Build the CLI, guest runtime, or example component via Crane and Nix without setup:

```bash
# Build the perry-wit CLI binary
nix build .#perry-wit

# Build the precompiled guest runtime
nix build .#guest-runtime

# Build the example WASIp2 component hermetically
nix build .#example-merge-docs
```

The compiled component is written to `result/lib/perry_merge_docs.stripped.wasm`.

### Integration Tests

Run the end-to-end integration test (starts the mock HTTP server, tests network failure handling, and verifies concurrent HTTP fetching and merging):

```bash
./scripts/test_e2e.sh
```

### Unit Tests

Run the pure-Rust linker and componentization tests:

```bash
cargo test
```

### Flake Checks

Verify all derivations, formatting, and flake contracts:

```bash
nix flake check
```

### CLI

View CLI options:

```bash
cargo run --bin perry-wit -- --help
# or:
perry-wit --help
```

```text
Usage: perry-wit [OPTIONS] <input.ts>

Options:
  -o, --out <PATH>      Output WebAssembly file path
      --runtime <PATH>  Guest runtime WASM module path
      --wit <PATH>      WIT definition directory (default: 'wit')
      --world <NAME>    WIT world name to target (default: 'merge-docs')
      --core-only       Output linked Core WebAssembly without component encoding
  -h, --help            Print help information
```

Compile and run a TypeScript file:

```bash
# Compile to a WASIp2 component
perry-wit examples/merge_docs.ts -o dist/my_component.wasm

# Execute component with wasmtime
wasmtime run -S http=y -S inherit-network=y dist/my_component.wasm
```
