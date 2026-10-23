# Perry-WIT Handbook

Perry-WIT compiles static TypeScript ahead-of-time into standalone WebAssembly components targeting **WASI 0.3 (Preview 3)**. The compiler lowers TypeScript through Perry HIR and typed SSA MIR (`pliron`) straight into native WebAssembly instructions, emitting self-contained components with instant startup, single-page (`64 KiB`) baseline memory footprints, and native stackless coroutine suspension.

<div class="pw-nav-links">
  <a href="../index.html">Landing Page</a>
  <a href="../play/index.html">Browser Playground</a>
  <a href="../rust/perry_wit/api/index.html">Rust Embedding API</a>
  <a href="conformance.html">Conformance Matrix</a>
</div>

## Design Guarantees

| Guarantee | Mechanism |
| :--- | :--- |
| **Ahead-of-Time Codegen** | TypeScript functions lower directly to WebAssembly instructions and SSA registers without an embedded JavaScript VM. |
| **Native WASI 0.3 Async** | Promises, streams, and timers suspend and resume through stackless Component Model callbacks rather than guest polling loops. |
| **Static Contract Enforcement** | Source validation reconciles TypeScript exports and imports against the resolved WIT world before code generation begins. |
| **Single-Heap Ownership** | Managed records, UTF-8 strings, and Canonical ABI scratch buffers share a unified `talc` linear memory space with deterministic post-return reclamation. |
| **Stratified Numerics** | Unconstrained numbers evaluate as `f64`, WIT-constrained widths specialize to native `i32`/`i64` instructions, and intermediate integer arithmetic promotes to `i64` registers. |
| **Differential Verification** | Capability contracts execute identically against both a Node.js oracle and a resource-metered Wasmtime P3 host. |

## Handbook Structure

- **[Architecture](architecture.md)** covers the seven-stage lowering pipeline, the iterative `pliron` optimization loop, unified linear memory tiers, WASI 0.3 concurrency, and freestanding Rust guest helpers.
- **[Semantics](semantics.md)** specifies the static TypeScript execution model, value representations, exception propagation, asynchronous stream lifecycles, and the supported standard library surface.
- **[Numerical Tower](numerical-tower.md)** details numeric representation stratification, the mixed-width subsumption join lattice, `i64` intermediate register promotion, and `bigint` boundary rules.
- **[Authoring](authoring.md)** walks through building library components, CLI commands, and WASI 0.3 HTTP handlers alongside WIT type generation and Nix project scaffolding.
- **[Embedding](embedding.md)** documents in-memory compilation through the `perry_wit::api` Rust crate and the self-hosted `perry:compiler/api` WebAssembly component.
- **[Conformance](conformance.md)** explains the differential oracle methodology, linear fuel scaling ratchets, and the complete capability contract catalog generated from the test registry.
