# perry-wit

End-to-end demonstration compiling TypeScript to native **WebAssembly WASI Preview 2 (WASI 0.2.4)** components using the sub-crates of [PerryTS/perry](https://github.com/PerryTS/perry) (`perry-parser`, `perry-hir`, `perry-codegen-wasm`), native static linking (`wasm-merge`), and deterministic WASI Preview 2 WIT bindings.

---

## Overview & Architecture

Unlike typical JS-on-Wasm runtimes that embed dynamic bytecode interpreters (like QuickJS, SpiderMonkey, or Wasmi) at a cost of 1.4 MB to 15 MB, **perry-wit compiles TypeScript directly to native WebAssembly bytecode**.

The compiled Core Wasm module is statically linked with a lightweight C-ABI guest runtime (`crates/guest-runtime`) using `wasm-merge` (Binaryen), and then componentized using `wasm-tools component new`.

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
                 wasm-merge (Binaryen) with guest-runtime
                 (statically binds rt imports to shared linear memory)
                                     │
                                     ▼
       ┌──────────────────────────────────────────────────────────────┐
       │             WASI Preview 2 Component (Zero Interpreters)     │
       │                                                              │
       │   Canonical ABI entry: wasi:cli/run@0.2.6#run                │
       │   Execution: Native Cranelift JIT (No Wasmi / No QuickJS)    │
       │   Size: ~111 KB stripped                                     │
       │                                                              │
       │   Runtime Bridge:                                            │
       │     - Promise.all([fetch, fetch]) ──► concurrent wasi:http   │
       │     - Array destructuring         ──► NaN-box index access   │
       │     - Response.json()             ──► serde_json stream      │
       │     - Object splatting ({...a})   ──► native object_assign   │
       │     - console.log                 ──► wasi:cli/stdout        │
       └──────────────────────────────┬───────────────────────────────┘
                                     │
                             wasmtime wasip2
                                     │
                                     ▼
                      Terminal Output via WASIp2 Stdout
```

### Components

1. **Host Compiler CLI (`perry-wit`)**:
   - `perry-parser`: parses TypeScript syntax into an AST.
   - `perry-hir`: lowers AST to Perry's High-level Intermediate Representation.
   - `perry-codegen-wasm`: compiles HIR into a Core WebAssembly binary with the Perry NaN-boxed ABI (`string_new`, `mem_call`, `fetch_with_options`, etc.).
   - Applies HIR rewrites for object spread IIFEs into native `Expr::ObjectAssign` and rewrites untyped `.json()` calls to `NativeMethodCall`.
   - Synthesizes `wasi:cli/run@0.2.6#run` entry point and aligns linear memory to 32 pages (2 MB) for the merged runtime data segment.
2. **Guest Runtime (`crates/guest-runtime`)**:
   - Minimal C-ABI runtime module targeting `wasm32-unknown-unknown` with imported linear memory.
   - Unified handle storage (`JsHandle`) avoiding ID collisions across JSON values, arrays, and in-flight HTTP streams.
   - Non-blocking `wasi:http/outgoing-handler@0.2.4` dispatch allowing multiple outgoing HTTP requests to run concurrently in parallel on the host event loop.
   - Strict WASI resource drop ordering (dropping stream, body, response, pollable before future response) to satisfy Component Model lifetime invariants.
3. **Static Linker & Componentizer**:
   - `wasm-merge`: fuses the compiled TypeScript module (`env`) and the runtime (`rt`), automatically resolving all 200+ ABI imports into direct internal function calls on shared linear memory.
   - `wasm-tools component embed` & `wasm-tools component new`: packages the linked core module into a WASI Preview 2 component matching `wit/world.wit`.

---

## Binary Footprint & Size Comparison

| Architecture | Approach | Size (Stripped) | Overhead / Engines |
| :--- | :--- | :--- | :--- |
| **Componentize-JS** | SpiderMonkey | ~5 MB – 15 MB | Heavy JS engine |
| **Javy** | QuickJS | ~1.5 MB – 2.0 MB | In-wasm JS interpreter |
| **perry-wit (previous)** | Wasmi | ~1.4 MB | In-wasm WebAssembly interpreter |
| **perry-wit (current)** | **Native Ahead-of-Time** | **~111 KB** | **Zero interpreters, direct native code** |

---

## Concurrent Fetch & `Promise.all`

### Concurrency in WebAssembly
WebAssembly execution within a single instance is single-threaded. However, **network I/O is asynchronous and handled by the host (Wasmtime)**:

1. Calling `fetch(url1)` immediately dispatches `wasi:http/outgoing-handler::handle`, returning an in-flight `future-incoming-response` handle.
2. Calling `fetch(url2)` immediately dispatches the second request.
3. Both TCP handshakes, TLS negotiations, and HTTP downloads proceed in parallel on Wasmtime's Tokio event loop in the host.
4. `Promise.all([fetch1, fetch2])` ensures all in-flight requests are actively progressing concurrently before awaiting results.

### TypeScript Example: `examples/merge_docs.ts`

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

---

## Quick Start & Verification

### 1. Build the Component
Compile the TypeScript program to Core Wasm, compile the runtime, merge with `wasm-merge`, embed WIT, and componentize:

```bash
./scripts/build.sh
```

### 2. Run the End-to-End Test
Launches the background mock HTTP server (port 8080) and runs the component in Nix `wasmtime`:

```bash
./scripts/test_e2e.sh
```

### 3. Run Manually with Wasmtime
```bash
nix run nixpkgs#wasmtime -- run -S http=y -S inherit-network=y dist/perry_merge_docs.stripped.wasm
```

Expected output:
```json
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
```
