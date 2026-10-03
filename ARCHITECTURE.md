# System Architecture: perry-wit

`perry-wit` is an ahead-of-time (AOT) compiler that compiles TypeScript directly into WebAssembly Component Model components (targeting WASI Preview 3 / 0.3 with backward compatibility for WASI Preview 2 / 0.2) using a pure, LLVM-free WAFFLE SSA backend.

It eliminates in-Wasm JavaScript interpreters (such as QuickJS, SpiderMonkey, or Wasmi) as well as heavy LLVM/C++ toolchain dependencies (`inkwell`, `llvm-sys`, `binaryen`). The compiler lowers TypeScript Abstract Syntax Trees (AST) through High-Level Intermediate Representation (HIR) directly into WAFFLE Single Static Assignment (SSA) blocks, synthesizing WebAssembly Component Model canonical ABI lift/lower wrappers with real native stack suspension.

---

## 1. Architectural Principles

1. **Zero Runtime Interpreter (Pure AOT):**
   TypeScript is compiled directly to WebAssembly bytecodes. Primitive variables and control-flow values are carried purely through SSA values and block parameters, eliminating linear memory roundtrips. Complex aggregates (strings, records) reside in a deterministic, bump-allocated linear memory arena without garbage collection pauses.

2. **LLVM-Free & Inkwell-Free Toolchain:**
   The compiler backend relies entirely on pure Rust crates (`perry-parser`, `perry-hir`, `waffle`, `wasm-encoder`, `wasmparser`). It completely eliminates `llvm-sys`, `inkwell`, and external C++ tools (`wasm-merge`), ensuring instant builds, tiny compiler footprints, and full portability across macOS, Linux, and Windows.

3. **Direct HIR → WAFFLE SSA Lowering:**
   Instead of emitting pre-compiled bytecode templates or unoptimized stack machine instructions, Perry HIR constructs (assignments, branches, while loops, intra-module function calls) are lowered directly into WAFFLE basic blocks, SSA values, and block parameters. WAFFLE performs validation (`body.validate()`, `body.verify_reducible()`), constant folding, block parameter cleanup, and clean structured WebAssembly recovery.

4. **Native Component Model Async (WASI 0.3 / P3):**
   Async functions and delays (e.g. `waitFor(ms)`) compile to direct Component Model async canonical lower/lift calls (e.g. `wasi:clocks/monotonic-clock@0.3.0#wait-for`). Stack suspension is managed natively by the host (Wasmtime/Cranelift) via fibers/stack switching. Await points in the guest compile to explicit continuation basic blocks whose block parameters carry live variables across suspension points without guest polling loops.

5. **Dual Measurable Conformance:**
   The compiler enforces and measures two distinct contracts:
   - **Top-Down (ECMAScript / Node.js API subset):** Exact language semantics, evaluation order (strict left-to-right argument and operand evaluation), lexical scoping, and shadowing.
   - **Bottom-Up (WASI P3 / P2 Capability Invariants):** Strict capability containment, zero unneeded host imports, correct canonical ABI type marshalling, and clean resource cleanup.

6. **Dual Packaging Modes:**
   - **Standalone ("Fat") Components:** Embeds complete application logic, standard library intrinsics, and canonical adapters into a self-contained component.
   - **Modular ("Thin") Components:** Emits minimal task logic importing standard modular runtime interfaces, composable via standard Component Model tools.

7. **Hermetic, Nix-First Toolchain:**
   All dependencies (Rust toolchain, WASI 0.3 and 0.2 WIT contracts, `wasm-tools`, `wasmtime`) are pinned reproducibly in `flake.nix`.

---

## 2. End-to-End Compilation Pipeline

```
  ┌────────────────────────────────────────────────────────┐
  │                 TypeScript Source (.ts)                │
  └───────────────────────────┬────────────────────────────┘
                              │
                    [1. Parse AST (SWC/Oxc)]
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │               Perry AST & HIR Lowering                 │
  └───────────────────────────┬────────────────────────────┘
                              │
            [2. Binding & Identity Resolution]
            - Resolve imports, aliases, built-ins
            - Detect lexical shadowing of intrinsics
            - Preserve receiver & argument evaluation order
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │                 Resolved HIR Contract                  │
  └───────────────────────────┬────────────────────────────┘
                              │
              [3. Direct WAFFLE SSA Lowering]
              - Let / LocalSet -> SSA Values
              - While loops -> Loop header block parameters
              - If / Else -> Branch join block parameters
              - Intra-module calls & typed intrinsics
              - Await -> Continuation blocks with parameters
              - Verify reducible CFG & validate SSA
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │                    WAFFLE Module                       │
  └───────────────────────────┬────────────────────────────┘
                              │
             [4. Core WebAssembly Generation]
             - Structured control-flow recovery
             - Unused block removal & constant folding
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │             Core WebAssembly (guest.wasm)              │
  └───────────────────────────┬────────────────────────────┘
                              │
           [5. Component Framing & Canonical ABI]
           - Embed WASI 0.3 (P3) async clock adapter
           - Synthesize canonical lift/lower exports
           - Wire host imports & async ABI wrappers
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │         WASI 0.3 Component (*.wasm, header 0x1000d)    │
  └────────────────────────────────────────────────────────┘
```

### Stage 1: AST Parsing & HIR Construction
- The TypeScript source is parsed into an abstract syntax tree using SWC / Oxc AST structures.
- AST nodes are lowered into Perry High-Level Intermediate Representation (`perry-hir`).
- Language constructs (functions, expressions, statements) are transformed into structured HIR items.

### Stage 2: Binding & Identity Resolution (`src/waffle_backend/resolve.rs`)
- SWC resolves source binding identities before Perry lowers builtin names.
  Capability aliases become collision-free extern bindings with a separate typed operation map.
  `LowerCapability` supplies one pure plan for source validation, core signatures, and canonical import wiring.
  Clock and random implementations contain no invocation state; SSA, exceptions, values, and suspension remain shared.
- Before capability classification or code emission, module bindings are resolved:
  - Intrinsic signatures are validated (`waitFor`, `hostDouble`, `readChunk`, `byteAt`).
  - Module functions are registered to allow mutual and nested intra-module function calls.
  - Lexical shadowing is audited: any attempt by local variables or parameters to illegally shadow declared host intrinsics is rejected with precise diagnostics.
  - Strict left-to-right evaluation order of arguments and operands is established.

### Stage 3: Direct WAFFLE SSA Lowering (`src/waffle_backend/ssa.rs`)
- Perry HIR functions are lowered directly into `waffle::FunctionBody`:
  - **Primitive Values:** Variables of type `number` and `boolean` are maintained purely as WAFFLE SSA `Value`s (`Type::F64`, `Type::I32`).
  - **Assignments:** `Stmt::Let` and `Stmt::Expr(Expr::LocalSet(..))` update active local SSA bindings in the compilation context without linear memory writes.
  - **Loops (`Stmt::While`):** A loop header block is synthesized with block parameters for all in-scope mutable variables. The loop body updates these variables and branches back to the header passing new values. An exit block receives the final SSA values.
  - **Branches (`Stmt::If`):** An `if`/`else` generates a `then_block`, `else_block`, and a shared `join_block`. The `join_block` defines block parameters carrying the converged state of all live variables.
  - **Function Calls:** Both intra-module calls (`Expr::FuncRef`) and external intrinsic calls (`Expr::ExternFuncRef`) are lowered to direct `Operator::Call` operations.
  - **Async Suspension (`Expr::Await`):** Emits an intrinsic call followed by an explicit `resumed` continuation block. Block parameters on the continuation block capture live local variables (plus the return value of the awaited promise), ensuring state survives across suspension.
  - **Verification:** Every function body undergoes `body.validate()` and `body.verify_reducible()`.

### Stage 4: Core WebAssembly Generation
- WAFFLE compiles its SSA basic blocks into valid, structured WebAssembly bytecode (`waffle_mod.to_wasm_bytes()`).
- WAFFLE's backend reconstructs WebAssembly block nesting, optimizes away redundant block parameters, eliminates unreachable blocks, and folds constant operations.

### Stage 5: Component Framing & Canonical ABI (`src/waffle_backend/component.rs`)
- The core WebAssembly module is framed into a Component Model component:
  - **WASI 0.3 Clocks Adapter:** When `uses_p3_clocks` is detected, the component embeds a canonical adapter linking `wasi:clocks/monotonic-clock@0.3.0#wait-for` to the core guest module, converting milliseconds to nanoseconds.
  - **Canonical Lift & Lower:** Exported functions (e.g. `run: async func(input: f64) -> f64`) are lifted using canonical async ABI primitives.
  - **Import Wiring:** Host imports (e.g. `host-double`, `read-chunk`) are canonically lowered and wired to the guest's import table.

---

## 3. Dual Conformance Architecture

`perry-wit` enforces rigorous verification across two boundaries:

```
┌────────────────────────────────────────────────────────┐
│             TypeScript Source (Task / App)             │
└───────────────────────────┬────────────────────────────┘
                            │
              Top-Down Conformance Checks
              (ECMAScript semantics, eval order, shadowing)
                            │
┌───────────────────────────▼────────────────────────────┐
│                    WAFFLE Compiler                     │
└───────────────────────────┬────────────────────────────┘
                            │
             Bottom-Up Conformance Checks
             (WASI 0.3 & 0.2 ABI, suspension, resource leak)
                            │
┌───────────────────────────▼────────────────────────────┐
│            Wasmtime Host (P3 / P2 Runtimes)            │
└────────────────────────────────────────────────────────┘
```

1. **Top-Down Conformance:**
   - Evaluates TypeScript tasks against Node.js / V8 baseline execution.
   - Verifies arithmetic precision, operator precedence, boolean short-circuiting, and loop termination.
   - Proves left-to-right evaluation order and lexical scope integrity.

2. **Bottom-Up Host Conformance:**
   - Verifies that components execute correctly under standard Wasmtime host runtimes with native async support enabled (`wasm_component_model_async(true)`).
   - Validates that timers suspend execution without spinning CPU cycles (verified via Tokio `timeout` probes).
   - Audits emitted components to ensure unused capabilities (e.g. HTTP, filesystem, random) are completely pruned when not declared in the task contract.

---

## 4. Module Layout & Narrow `pub(crate)` Boundaries

The compiler is organized into decoupled functional Rust modules with narrow internal interfaces:

- `src/lib.rs`: Public API surface, re-exporting `compile_typescript_waffle`, `WaffleCompiled`, and `WaffleCompileOptions`.
- `src/waffle_backend/mod.rs`: Top-level orchestration module coordinating parsing, resolution, SSA lowering, and componentization. Retains inspectable artifacts (`hir`, `waffle_ir`, `core`, `component_wat`, `component`).
- `src/waffle_backend/audit.rs`: `pub(crate)` dependency auditor verifying that the compiler build and dependency graph contains zero LLVM or inkwell libraries.
- `src/waffle_backend/resolve.rs`: `pub(crate)` contract and binding resolution module. Classifies typed intrinsics, checks shadowing, and enforces function contracts.
- `src/waffle_backend/capabilities/`: Binding-aware source normalization and typed capability plans, grouped by clock and random responsibility.
- `src/waffle_backend/ssa.rs`: `pub(crate)` direct HIR-to-WAFFLE SSA lowering engine. Constructs basic blocks, SSA values, loop headers, branch joins, and continuation blocks.
- `src/waffle_backend/component.rs`: `pub(crate)` component model framing engine synthesizing WASI 0.3 / P3 async adapters and canonical ABI lift/lower bindings.

---

## 5. Summary of Key Invariants

| Attribute | Legacy Pipeline (`perry-codegen-wasm`) | Target Pipeline (`perry-wit` WAFFLE) |
|---|---|---|
| **Backend Code Generator** | LLVM / Inkwell / C++ | Pure Rust WAFFLE SSA |
| **Control Flow** | Stack-based or LLVM CFG | Direct WAFFLE Basic Blocks + Block Parameters |
| **Variable Storage** | Linear memory loads/stores | Pure SSA Values (`f64`, `i32`) |
| **Async Execution** | In-guest event pump / polling | Native WASI 0.3 Component Model Async / Stack Suspension |
| **Clocks Interface** | WASI Preview 1 / Preview 2 sync polling | `wasi:clocks/monotonic-clock@0.3.0` async `wait-for` |
| **Module Linking** | External Binaryen `wasm-merge` | In-process Component Model composition |
| **Toolchain Dependencies** | Heavy LLVM, C++ binaries, Python scripts | Pure Rust + Nix hermetic toolchain |
