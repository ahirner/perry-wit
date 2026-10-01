# perry-wit

Hermetic TypeScript compiler and WASI Preview 2 component toolchain. Compiles TypeScript directly to native **WebAssembly WASI Preview 2 (WASI 0.2.6)** components using [Perry](https://github.com/PerryTS/perry), a pure-Rust in-process module linker, and dynamic WASI Preview 2 WIT contracts.

---

## Overview & Architecture

Unlike JS-on-Wasm runtimes that embed dynamic bytecode interpreters (QuickJS, SpiderMonkey, or Wasmi) at a cost of 1.4 MB to 15 MB, **perry-wit compiles TypeScript directly to native WebAssembly bytecode**.

The compilation pipeline operates entirely in-process using pure Rust, with **zero external binary dependencies** (no Binaryen, `wasm-merge`, or external CLI tools):

```
                       ┌────────────────────────────┐
                       │   examples/merge_docs.ts   │
                       └─────────────┬──────────────┘
                                     │
                perry-parser / perry-hir / perry-codegen-wasm
                                     │
                                     ▼
                       ┌────────────────────────────┐
                       │   Core WebAssembly Module  │ (~13 KB)
                       │   (Perry NaN-boxed ABI)    │
                       └─────────────┬──────────────┘
                                     │
                perry_wit::linker::merge_core_modules
                (pure-Rust static linker, remapping & resolving
                 200+ rt imports to shared linear memory)
                                     │
                                     ▼
       ┌──────────────────────────────────────────────────────────────┐
       │             WASI Preview 2 Component (Zero Interpreters)     │
       │                                                              │
       │   Canonical ABI entry: wasi:cli/run@0.2.6#run                │
       │   Execution: Native Cranelift JIT (No Wasmi / No QuickJS)    │
       │   Size: ~118 KB stripped                                     │
       │                                                              │
       │   Runtime Bridge:                                            │
       │     - Promise.all([fetch, fetch]) ──► concurrent wasi:http   │
       │     - Array destructuring         ──► NaN-box index access   │
       │     - Response.json()             ──► serde_json stream      │
       │     - Object splatting ({...a})   ──► native object_assign   │
       │     - console.log                 ──► wasi:cli/stdout        │
       └──────────────────────────────┬───────────────────────────────┘
                                     │
                            wasmtime wasip2 (>= 48.0)
                                     │
                                     ▼
                      Terminal Output via WASIp2 Stdout
```

### Key Components

1. **Host Compiler CLI (`perry-wit`)**:
   - `perry-parser`: Parses TypeScript source code into an AST.
   - `perry-hir`: Lowers AST to Perry High-Level Intermediate Representation.
   - `perry-codegen-wasm`: Compiles HIR into a Core WebAssembly binary with Perry's NaN-boxed ABI (`string_new`, `mem_call`, `fetch_with_options`, etc.).
   - Applies HIR rewrites for object spread IIFEs into native `Expr::ObjectAssign` and rewrites `.json()` calls to `NativeMethodCall`.
   - Synthesizes `wasi:cli/run@0.2.6#run` entry point and aligns linear memory to 32 pages (2 MB).
2. **Pure-Rust In-Process Linker (`perry_wit::linker`)**:
   - Statically fuses the compiled TypeScript module (`env`) and the guest runtime module (`rt`) in memory.
   - Eliminates all C/C++ Binaryen (`wasm-merge`) dependencies.
   - Remaps function, type, and global indices, binds linear memory, and resolves all runtime function calls into direct internal calls.
3. **Pure-Rust Componentization & Stripping (`perry_wit::component`)**:
   - Embeds WIT interfaces and world declarations into the linked Core Wasm module using `wit-component`.
   - Dynamically resolves WASI Preview 2 WIT packages in strict topological order (`io` $\rightarrow$ `random` $\rightarrow$ `clocks` $\rightarrow$ `filesystem` $\rightarrow$ `sockets` $\rightarrow$ `cli` $\rightarrow$ `http`).
   - Strips debug and producer custom sections via `wasm-encoder`, producing a lean ~118 KB component.
4. **Guest Runtime (`crates/guest-runtime`)**:
   - Minimal C-ABI runtime module targeting `wasm32-unknown-unknown` with imported linear memory.
   - Unified handle storage (`JsHandle`) avoiding ID collisions across JSON values, arrays, and in-flight HTTP streams.
   - Non-blocking `wasi:http/outgoing-handler@0.2.6` dispatch allowing multiple outgoing HTTP requests to run concurrently on the host event loop.
   - Strict WASI resource drop ordering satisfying Component Model lifetime invariants.

---

## Binary Footprint Comparison

| Architecture | Approach | Size (Stripped) | Overhead / Runtime Engines |
| :--- | :--- | :--- | :--- |
| **Componentize-JS** | SpiderMonkey | ~5 MB – 15 MB | Heavy JS engine |
| **Javy** | QuickJS | ~1.5 MB – 2.0 MB | In-wasm JS interpreter |
| **perry-wit (previous)** | Wasmi | ~1.4 MB | In-wasm WebAssembly interpreter |
| **perry-wit (current)** | **Native Ahead-of-Time** | **~118 KB** | **Zero interpreters, direct native code** |

---

## Hermetic Nix Foundation & Modern Toolchain

The repository includes a modern, hermetic Nix Flake (`flake.nix`):

- **Toolchain**: Pinned via `rust-toolchain.toml` with `wasm32-unknown-unknown` and `wasm32-wasip2` targets.
- **Wasmtime**: Guaranteed `wasmtime >= 48.0` (48.0.1) from `nixpkgs-unstable`.
- **Node.js**: Modern Node.js (`v24.21.0`).
- **Dynamic WASI Preview 2 WIT**: Official definitions are sourced as a flake input (`github:WebAssembly/WASI/v0.2.6`) and provided via `WASI_WIT_PATH`. No uncommitted or vendored WIT definitions in version control.

---

## How to Use & Test

### Option 1: Hermetic Nix Build (Zero Setup)

Build the CLI, guest runtime, or example component with a single command from cold in seconds:

```bash
# 1. Build the perry-wit CLI binary
nix build .#perry-wit

# 2. Build the precompiled guest runtime
nix build .#guest-runtime

# 3. Build the example WASIp2 component hermetically
nix build .#example-merge-docs
```

The compiled component is placed in `result/lib/perry_merge_docs.stripped.wasm`.

### Option 2: Run End-to-End Test Suite

Run the full end-to-end integration test (spins up a mock HTTP server, tests network failure fail-safe, and validates concurrent HTTP document merging):

```bash
# Inside nix devShell (or if wasmtime >= 48 is installed locally)
./scripts/test_e2e.sh
```

Or run via `nix develop`:

```bash
nix develop --command ./scripts/test_e2e.sh
```

Expected output:
```text
==> Verifying component fails with error when server is not running...
==> Verified: Component exits with error when server is down as expected.
==> Starting mock HTTP server on 127.0.0.1:8080...
==> Executing WASIp2 component with wasmtime (with -S http=y -S inherit-network=y)...
=== MERGED DOCUMENT (SPLATTED) ===
{
  "author": "WebAssembly Community Group",
  "category": "wasm-preview2",
  "description": "Initial draft compiled with Perry TypeScript",
  "draft": false,
  "id": "doc-alpha",
  "name": "Component Document",
  "tags": [
    "perry",
    "wasip2",
    "wit"
  ],
  "verified": true,
  "version": 2
}
==================================
==> Verifying output assertions...
==> SUCCESS: End-to-end WASIp2 component execution verified!
```

### Option 3: Run Rust Unit & Integration Tests

```bash
cargo test
```

Or inside the hermetic Nix environment:

```bash
nix develop --command cargo test
```

### Option 4: Run Flake Verification Check

Verify all derivations, formatting, and flake contracts:

```bash
nix flake check
```

### Option 5: Compile Any TypeScript File with `perry-wit` CLI

Enter the devShell or run the built binary directly:

```bash
nix develop
```

View CLI help:
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
# Build component
cargo run --bin perry-wit -- examples/merge_docs.ts -o dist/my_component.wasm

# Execute component with wasmtime
wasmtime run -S http=y -S inherit-network=y dist/my_component.wasm
```

---

## TypeScript Conformance & Concurrent `Promise.all`

WebAssembly instance execution is single-threaded, but **network I/O is asynchronous and non-blocking on the host (Wasmtime)**:

1. `fetch(url1)` immediately dispatches `wasi:http/outgoing-handler::handle`, returning an in-flight `future-incoming-response` handle.
2. `fetch(url2)` immediately dispatches the second request.
3. Both requests stream concurrently on Wasmtime's Tokio event loop in the host.
4. `Promise.all([fetch1, fetch2])` awaits both in-flight futures before parsing responses with `.json()`.

```typescript
// 1. Initiate BOTH HTTP requests concurrently using Promise.all
const [res1, res2] = await Promise.all([
    fetch("http://127.0.0.1:8080/doc1.json"),
    fetch("http://127.0.0.1:8080/doc2.json")
]);

// 2. Parse JSON documents
const doc1 = await res1.json();
const doc2 = await res2.json();

// 3. Merge using object splatting
const merged = { ...doc1, ...doc2 };

console.log("=== MERGED DOCUMENT (SPLATTED) ===");
console.log(JSON.stringify(merged));
console.log("==================================");
```
