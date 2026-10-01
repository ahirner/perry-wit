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

- [x] **2.1. Root `flake.nix` with Modern Toolchain**
  - [x] Use `nixpkgs-unstable` to guarantee `wasmtime >= 48.0` (48.0.1) and latest `nodejs` (24.21.0).
  - [x] Provide devShell with: `rust-bin` (pinned Rust), `wasmtime`, `wasm-tools`, `nodejs`, `wkg`.
- [x] **2.2. Dynamic WASI Preview 2 WIT Sourcing**
  - [x] Source official WASI Preview 2 WIT definitions as a flake input (`github:WebAssembly/WASI/v0.2.6`).
  - [x] In `shellHook` and build derivations, expose the WIT package directory via `WASI_WIT_PATH` with consolidated `package.wit`.
  - [x] Untrack and git-ignore `wit/deps/` from the repository; dynamically resolve packages in strict topological dependency order.
- [x] **2.3. Single-Command Hermetic Build**
  - [x] Verify `nix build` successfully builds the CLI (`.#perry-wit`), guest runtime (`.#guest-runtime`), and example component (`.#example-merge-docs`) hermetically from cold in seconds.

---

## Phase 3: CLI Example Component Differential Validation (Node.js vs. WASIp2 CLI)

*Goal: Formally measure and validate end-to-end command execution behaviors and stream outputs of example components compiled to `wasi:cli/command` against native Node.js oracle and WASI Preview 2 host invariants.*

- [x] **3.1. Machine-Readable Capability Catalog (`catalog/capabilities.json` & `src/conformance/catalog.rs`)**
  - [x] Specify formal capability schema:
    - Unique capability IDs, names, tiers (Tier 1 Web Primaries, Tier 2 Node Core, Non-Goals).
    - Support status: `full`, `partial`, `unsupported`.
    - Explicit compatibility domain boundaries, constraints, and invariants.
    - Test case mapping linking each capability to formal validation evidence.
  - [x] Implement Rust catalog parser and validator in `src/conformance/catalog.rs` enforcing contract integrity.
- [x] **3.2. Example Component CLI Validation Cases (`tests/conformance/cases/`)**
  - [x] Implement isolated capability-level validation cases (distinct from application-level examples):
    - `01_object_spread.ts`: Object spread `{ ...a, ...b }` precedence, property overrides, key enumeration.
    - `02_console_streams.ts`: Distinct standard stream routing (`console.log` -> stdout, `console.error` -> stderr).
    - `03_promise_all.ts`: Concurrent promise resolution ordering and value aggregation.
    - `04_fetch_json.ts`: HTTP GET response streaming, status check, and `.json()` structured object decoding.
    - `05_fetch_failure.ts`: Network connection failure rejection and diagnostic reporting.
    - `06_json_syntax.ts`: Parse error boundary and malformed payload rejection.
- [x] **3.3. Pure-Rust Differential Equivalence Harness (`src/conformance/runner.rs`)**
  - [x] Execute each validation case under reference oracle (`node`) and under Perry (`perry-wit` -> `wasmtime`).
  - [x] Compare execution vectors: exit code, stdout stream, stderr stream, and structured JSON output.
  - [x] Run automated hermetic mock server for HTTP validation cases.
- [x] **3.4. Host Invariants & Validation Reporting (`src/conformance/report.rs` & `tests/conformance_test.rs`)**
  - [x] Verify WASI Preview 2 host invariants:
    - Zero resource leaks on completion (clean drop of all streams and pollables).
    - Layered exit code translation: uncaught exceptions map to `wasi:cli/exit` status 1 without host memory corruption.
  - [x] Generate structured validation report mapping catalog declarations to executed evidence.

---

## Phase 4: Component Tasks (Exported Functions & Canonical ABI)

*Goal: Support standard TypeScript function exports (`export function runTask(...)`) directly as Component Model exports with typed inputs and outputs according to WIT specifications, enabling direct host task invocation (`wasmtime --invoke`).*

- [x] **4.1. Guest Runtime Canonical ABI Memory & String Primitives (`crates/guest-runtime/src/cabi.rs`)**
  - [x] Implement `cabi_import_string(ptr: i32, len: i32) -> i64` unpacking UTF-8 slices to nanboxed JS string handles.
  - [x] Implement `cabi_export_string(val: i64) -> i32` allocating Canonical ABI 8-byte ret areas `[ptr, len]` and UTF-8 bytes.
  - [x] Implement `cabi_export_result_string(val: i64, is_err: i32) -> i32` for `result<string, string>` / variant returns.
  - [x] Implement `cabi_import_json(ptr: i32, len: i32) -> i64` and `cabi_export_json(val: i64) -> i32` for structured records.
  - [x] Export `cabi_realloc` for guest memory allocation requested by external host callers.
- [ ] **4.2. WIT Export Inspection & Function Mapping (`src/abi/wit_meta.rs`)**
  - [ ] Inspect WIT world exports via `wit_parser` to extract exported function signatures (names, params, results).
  - [ ] Map TypeScript AST/HIR `exported_functions` to corresponding WIT world export functions (handling exact matches and camelCase <-> kebab-case).
  - [ ] Verify contract compatibility between TypeScript function parameters and WIT function signatures.
- [ ] **4.3. Canonical ABI Trampoline Synthesizer (`src/abi/trampoline.rs` & `src/compiler/wasi.rs`)**
  - [ ] Synthesize typed `$cabi_*` entrypoints in core WebAssembly for each matched exported function.
  - [ ] Unpack parameters from Canonical ABI to nanboxed JS representations.
  - [ ] Invoke Perry's compiled function index (`__wasm_func_<idx>`).
  - [ ] Serialize result value into Canonical ABI return area.
  - [ ] Preserve backwards compatibility for `wasi:cli/run` when `wasi:cli/command` is present.
- [ ] **4.4. Component Task Examples & Direct Invocation Test Suite (`tests/task_invocation_test.rs`)**
  - [ ] Author task component `examples/merge_task.ts` taking structured `MergeInput` record / string and returning merged document.
  - [ ] Add `world task-runner` and `world merge-task` to `wit/world.wit`.
  - [ ] Author integration test suite testing direct invocation via `wasmtime run --invoke 'run-task'` and typed component execution.
  - [ ] Verify clean resource drops and error boundary semantics on direct task invocation.
- [ ] **4.5. Flake & Build Integration**
  - [ ] Add `exampleMergeTask` package and check to `flake.nix`.
  - [ ] Update `scripts/build.sh` to compile and verify task components end-to-end.

---

## Phase 5: Zero-Config SDK Experience

*Goal: An author provides only a `world.wit`; entering the Nix shell automatically generates TypeScript declarations (`.d.ts`), configures `tsconfig.json`, and enables instant IDE type-checking.*

- [ ] **5.1. WIT to TypeScript Declaration Generator (`perry-wit gen-types`)**
  - [ ] Implement CLI subcommand and generator module creating `.perry/types/world.d.ts` from any given `world.wit`.
  - [ ] Map WIT records to TS interfaces, variants to discriminated unions, lists/options to TS arrays/nullables, and exported functions to typed declarations.
- [ ] **5.2. Wire DevShell Automation**
  - [ ] In `flake.nix`, configure `shellHook` to detect `world.wit`, run the type generator, link `tsconfig.json`, and expose tooling in `$PATH`.
  - [ ] Provide pre-commit / flake check validating that generated TypeScript definitions remain in sync with WIT definitions.
- [ ] **5.3. Validate Builtin Examples Against SDK**
  - [ ] Run `tsc --noEmit` on all examples inside the Nix shell to prove zero-error static typing against generated WIT contracts.
