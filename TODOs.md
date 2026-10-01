# TODOs: Perry-WIT Roadmap

This checklist prioritizes immediate functional deliverables and eliminates code churn: it replaces Binaryen with a pure-Rust in-process module linker upfront, establishes a hermetic Nix environment with modern tooling (`wasmtime >= 48`, `nodejs`) and uncommitted WASI WIT files, and builds the dual-conformance evaluation loop.

---

## Phase 1: Pure-Rust In-Process Module Linker (Kill Binaryen Upfront)

*Goal: Make `perry-wit` a self-contained Rust toolchain that compiles TypeScript and links the guest runtime directly in-process without external C++ tools (`wasm-merge`).*

- [x] **1.1. In-Process Core Module Merger (`src/linker/` or `crates/perry-linker/`)**
  - [x] Implement an in-process Rust linker using `wasmparser` and `wasm-encoder`:
    - Merge linear memory declarations (assigning shared memory with unified page limits).
    - Remap function, type, and global indices between the TypeScript core wasm and `guest-runtime.wasm`.
    - Offset and combine data segments.
    - Resolve and link `(import "rt" ...)` calls to runtime export functions directly.
- [x] **1.2. Integrate Linker into `perry-wit` CLI**
  - [x] Embed or dynamically link `guest-runtime.wasm` directly within the `perry-wit` build pipeline.
  - [x] Run `wasm-tools component new` natively via its Rust API (`wit-component`) rather than external CLI invocations.
  - [x] Validate that `cargo run -- examples/merge_docs.ts -o dist/perry_merge_docs.stripped.wasm` builds the component end-to-end with zero external tool dependencies.
- [x] **1.3. Retire `wasm-merge` and Update Scripts**
  - [x] Remove `wasm-merge` from `scripts/build.sh`.
  - [x] Remove Binaryen dependencies from Nix expressions.

---

## Phase 2: Hermetic Nix Foundation (Uncommitted WASI WIT & Modern Tooling)

*Goal: Provide a reproducible, idiomatic Nix environment supplying pinned WASI WIT definitions from Nix store inputs without committing them to git, with `wasmtime >= 48` and `nodejs`.*

- [ ] **2.1. Root `flake.nix` with Modern Toolchain**
  - [ ] Use `nixpkgs-unstable` to guarantee `wasmtime >= 48.0` and latest `nodejs`.
  - [ ] Provide devShell with: `rust-bin` (pinned Rust), `wasmtime`, `wasm-tools`, `nodejs`.
- [ ] **2.2. Dynamic WASI Preview 2 WIT Sourcing**
  - [ ] Source official WASI Preview 2 WIT definitions as a flake input (e.g. `github:WebAssembly/WASI/v0.2.4` or `v0.2.6`).
  - [ ] In `shellHook` and build derivations, expose the WIT package directory via an environment variable (`WASI_WIT_PATH`) or standard path mapping.
  - [ ] Untrack and remove committed `wit/deps/` from the repository; keep git history clean and idiomatic.
- [ ] **2.3. Single-Command Hermetic Build**
  - [ ] Verify `nix build` successfully builds the CLI and example components from cold in seconds.

---

## Phase 3: The Conformance Evaluation Loop (Node.js vs. WASIp2)

*Goal: Quantifiably measure how well the compiled component matches promised Node.js APIs and WASI host invariants.*

- [ ] **3.1. Tiered Capability Catalog (`catalog/capabilities.json`)**
  - [ ] Document initial active surface:
    - **Tier 1 (Web Primaries):** `global.fetch` (HTTP GET/POST, headers, streaming body), `Promise.all` (concurrent dispatch, fail-fast), `response.json()` (UTF-8 parsing into typed object), `console.log` / `console.error` (stdout/stderr routing), Object splatting `{ ...a, ...b }`.
    - **Tier 2 (Node Core):** `process.exit`, `process.env`.
  - [ ] Define explicit domain boundaries (e.g. local socket binding constraints, redirect policies).
- [ ] **3.2. Author Initial Conformance Test Suite (`tests/conformance/`)**
  - [ ] `01_fetch_success.ts`: Concurrent `Promise.all` fetching multiple JSON endpoints and merging.
  - [ ] `02_fetch_connection_refused.ts`: Verify connection failure rejects promise with descriptive error.
  - [ ] `03_fetch_404_error.ts`: Non-2xx HTTP status handling in `.json()` and error inspection.
  - [ ] `04_console_routing.ts`: Verify `console.log` writes to stdout and `console.error` writes to stderr.
  - [ ] `05_json_syntax_error.ts`: Parsing invalid JSON string; assert fail-safe rejection.
- [ ] **3.3. Differential Equivalence Test Runner (`scripts/test_conformance.sh` or `tests/conformance.rs`)**
  - [ ] Execute each test against native Node.js (`node`) and against compiled WASIp2 component (`wasmtime`).
  - [ ] Assert matching execution vectors: stdout, stderr, exit code, and structured JSON output.
  - [ ] Generate structured conformance report table with pass/fail metrics.
- [ ] **3.4. Host Invariant & Resource Leak Verification**
  - [ ] Verify that all WASI resources (`future-incoming-response`, `incoming-response`, `input-stream`) are closed and dropped after execution (0 leaked descriptors).
  - [ ] Verify layered error handling:
    - Component boundary: Rejections surface as catchable errors without instance memory corruption.
    - Command boundary: Uncaught errors terminate via `wasi:cli/exit@0.2.x` with exit status 1.

---

## Phase 4: Component Tasks (Exported Functions & Canonical ABI)

*Goal: Support standard TypeScript function exports (`export function runTask(...)`) directly as Component Model exports with typed inputs and outputs.*

- [ ] **4.1. Export Function Identification & Lowering**
  - [ ] Detect `ExportNamedDeclaration` in Perry's AST/HIR.
  - [ ] Map exported TS functions to corresponding exported functions in `world.wit`.
- [ ] **4.2. Canonical ABI Trampolines**
  - [ ] Synthesize `$cabi_*` entrypoints unpacking typed arguments (records, strings, numbers) from linear memory.
  - [ ] Lower return values into Canonical ABI result memory and emit `$cabi_post_*` cleanup hooks.
- [ ] **4.3. Author Example Task Component**
  - [ ] Add `examples/merge_task.ts` taking a structured `MergeInput` record and returning a structured `MergedDoc`.
  - [ ] Verify end-to-end execution directly via Wasmtime component invocation (`wasmtime run --invoke 'merge-docs(...)'`).

---

## Phase 5: Zero-Config SDK Experience

*Goal: An author provides only a `world.wit`; entering the Nix shell automatically generates TypeScript declarations (`.d.ts`), configures `tsconfig.json`, and enables instant IDE type-checking.*

- [ ] **5.1. WIT to TypeScript Declaration Generator**
  - [ ] Implement automatic generation of `.perry/types/world.d.ts` from any given `world.wit`.
  - [ ] Map WIT records to TS interfaces, variants to discriminated unions, and exported functions to typed declarations.
- [ ] **5.2. Wire DevShell Automation**
  - [ ] In `flake.nix`, configure `shellHook` to detect `world.wit`, run the type generator, link `tsconfig.json`, and expose tooling in `$PATH`.
- [ ] **5.3. Validate Builtin Examples Against SDK**
  - [ ] Run `tsc --noEmit` on all examples inside the Nix shell to prove zero-error static typing against generated WIT contracts.
