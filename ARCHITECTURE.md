# System Architecture: perry-wit

`perry-wit` is an ahead-of-time (AOT) optimizing compiler that compiles TypeScript directly into WebAssembly Preview 2 components with native WIT (WebAssembly Interface Type) imports and exports.

It eliminates in-Wasm JavaScript interpreters (such as QuickJS, SpiderMonkey, or Wasmi), lowering TypeScript Abstract Syntax Trees (AST) through High-Level Intermediate Representation (HIR) straight to Cranelift-executable WebAssembly. The result is instant startup, minimal memory overhead, tiny binary footprints (~10 KB modular or ~118 KB standalone), and native WASI Preview 2 I/O.

---

## 1. Architectural Principles

1. **Zero Runtime Interpreter (Pure AOT):**
   TypeScript is compiled directly to Core WebAssembly bytecodes. Memory structures, objects, strings, and closures are managed in linear memory via an ephemeral, high-throughput arena and NaN-boxed values without garbage collection pauses.
2. **Dual Measurable Conformance:**
   The system explicitly distinguishes and measures two independent contracts:
   - **Top-Down:** Promised conformance to ECMAScript and Node.js standard APIs (`fetch`, `Promise.all`, `console`, `JSON`, `URL`, `process`, `fs`).
   - **Bottom-Up:** Conformance to WASI Preview 2 host capability invariants (`wasi:http`, `wasi:cli`, `wasi:io`, `wasi:clocks`, `wasi:random`).
3. **Decoupled Packaging Modes (Modular vs. Standalone):**
   - **Standalone ("Fat") Components:** Embeds the TypeScript logic and guest runtime adapter into a single, self-contained WASIp2 component runnable on any vanilla host.
   - **Modular ("Thin") Components:** Emits a lightweight task component importing a standardized runtime interface (`perry:runtime` or standard WIT), linked dynamically at runtime by the host (such as `example-host`) or composed via `wac`.
4. **Hermetic, Nix-First Toolchain:**
   All dependencies (Rust toolchain, WASI WIT contracts, `wasm-tools`, `wasmtime`) are hermetically fetched and pinned by Nix. WIT contract definitions are supplied from Nix derivations rather than committed to git history.
5. **Zero-Config Developer SDK:**
   Task authors provide only a `world.wit`. Entering the Nix development shell automatically generates exact TypeScript declarations (`.d.ts`), configures `tsconfig.json`, and injects SDK shims for local testing and IDE autocompletion.

---

## 2. Compilation & Execution Pipeline

```
  ┌────────────────────────────────────────────────────────┐
  │                 TypeScript Source (.ts)                │
  └───────────────────────────┬────────────────────────────┘
                              │
                    [1. Parse AST & Check]
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │                 Perry HIR (Typed SSA)                  │
  └───────────────────────────┬────────────────────────────┘
                              │
                    [2. Linear Memory Plan]
                    (Arena layout, NaN-boxing)
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │         Core WebAssembly Module (app.core.wasm)        │
  └───────────────────────────┬────────────────────────────┘
                              │
               [3. Module Linker (Pure Rust)]
               (Fuses app logic + guest-runtime)
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │              Linked Core WebAssembly Module            │
  └───────────────────────────┬────────────────────────────┘
                              │
            [4. Component Embed & Canonical ABI]
            (Embeds WIT contracts, binds exports)
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │         WASIp2 Component (*.wasm, header 0x1000d)      │
  └────────────────────────────────────────────────────────┘
```

### Stage 1: Parse and Lower to HIR
- Parses TypeScript syntax (using SWC/Oxc parser foundation).
- Translates control flow, variables, function applications, and object manipulations into Perry High-Level Intermediate Representation (HIR).
- Identifies exports:
  - Top-level scripts are routed to `wasi:cli/run@0.2.x#run`.
  - Named exports (`export function runTask(...)`) are recorded as candidate component export targets.

### Stage 2: Memory Planning & Core Wasm Emission
- Allocates linear memory layout: static literal pools, global descriptors, execution stack, and the dynamic heap arena.
- Represents JavaScript values via NaN-boxing:
  - Doubles: Standard IEEE-754 floats.
  - Pointers: Tagged bitmask (`0x7FFD`) addressing objects, arrays, and handles.
  - Strings: Tagged bitmask (`0x7FFF`) addressing UTF-8 string tables.
  - Primitives: Specific sentinel bit patterns for `undefined`, `null`, `true`, and `false`.
- Emits Core WebAssembly bytecodes calling internal runtime operations under the `rt` import namespace.

### Stage 3: Module Linking
- Merges the compiled TypeScript core module with the guest runtime adapter (`guest-runtime`).
- In-process Rust linker (using `wasm-encoder` and `wasmparser`), remapping function/type indices and resolving data segments in memory without spawning external C++ processes (**Initial Implementation:** Performed via Binaryen's `wasm-merge` on shared linear memory).


### Stage 4: Componentization & Canonical ABI Synthesis
- Embeds WIT package definitions into custom sections (`component-type:*`).
- Synthesizes Canonical ABI lift/lower adapters:
  - Memory reallocator (`cabi_realloc`).
  - Type encoding/decoding between external WIT types and linear memory.
  - Final validation via `wasm-tools component new` and `wasm-tools validate --features cm-async`.

---

## 3. Dual Conformance Architecture

To ensure measurable reliability, `perry-wit` validates behavior on two separate boundaries:

```
┌────────────────────────────────────────────────────────┐
│             TypeScript Source (Task / App)             │
└───────────────────────────┬────────────────────────────┘
                            │
               [A. Promised Node.js APIs]
       (Tiered: Web Primaries, Node Builtins, Streams)
                            │
┌───────────────────────────▼────────────────────────────┐
│                  Perry Compiler + RT                   │
└───────────────────────────┬────────────────────────────┘
                            │
          [B. WASI Preview 2 Capability Invariants]
     (wasi:http, wasi:cli, wasi:io, wasi:clocks, streams)
                            │
┌───────────────────────────▼────────────────────────────┐
│            WASI Preview 2 Host (Wasmtime)              │
└────────────────────────────────────────────────────────┘
```

### Layer A: Promised Conformance to Node.js / Web Standards (Top-Down)
Every supported language construct and runtime API is documented in a machine-readable capability catalog (`catalog/capabilities.json`), organized into distinct functional tiers:

1. **Tiered API Surface:**
   - **Tier 1 (Web Standard Primaries & ECMAScript Globals):** Universal primitives available globally: `fetch`, `Headers`, `Request`, `Response`, `URL`, `URLSearchParams`, `console`, `TextEncoder`, `TextDecoder`, `Promise` (`all`, `allSettled`, `race`), `JSON`, `crypto.getRandomValues`, `Date`, `Map`, `Set`, `RegExp`.
   - **Tier 2 (Node Core Builtins Mapped to WASI Capabilities):** Platform modules backed by WASI Preview 2 host capabilities:
     - `node:process`: `env`, `argv`, `cwd()` $\rightarrow$ `wasi:cli/environment`.
     - `node:fs` / `node:fs/promises`: `readFile`, `writeFile`, `stat` $\rightarrow$ `wasi:filesystem/preopens` & `types`.
     - `node:crypto`: `randomUUID()`, `getRandomValues()` $\rightarrow$ `wasi:random/random`.
     - `node:timers`: `setTimeout`, `clearTimeout` $\rightarrow$ `wasi:clocks/monotonic-clock` & `wasi:io/poll`.
     - `node:buffer`: `Buffer` operations over linear memory byte slices.
   - **Tier 3 (Node Stream & Event Mechanics):**
     - `node:stream`: `ReadableStream`, `WritableStream` $\rightarrow$ `wasi:io/streams`.
     - `node:events`: `EventEmitter`.
   - **Non-Goals / Explicitly Excluded from AOT Scope:** APIs requiring a dynamic bytecode evaluator or unconstrained OS threading/process forks (`eval()`, `vm`, `v8`, `child_process`).

2. **Classification Levels:**
   - `full`: Complete behavioral parity with recent Node.js LTS.
   - `partial`: Conforming subset with explicit boundary constraints (e.g. `fs.readFile` limited to preopened directory descriptors; `fetch` without arbitrary loopback redirects).
   - `unsupported`: Detected and diagnosed at compile time.

3. **Differential Equivalence Harness:**
   A test runner executes identical test suites across two engines:
   - **Reference:** Native Node.js (`node --test`).
   - **System Under Test (SUT):** `perry-wit` $\rightarrow$ `wasmtime run`.
   The runner asserts identical stdout, stderr, returned structured records, and exception semantics.

### Layer B: WASI Preview 2 Capability Invariants (Bottom-Up)
Verifies that the generated WebAssembly component strictly respects WASI 0.2 runtime specifications:
1. **Resource Lifecycle & Zero-Leak Invariant:**
   - All `wasi:http` resources (`future-incoming-response`, `incoming-response`, `input-stream`, `outgoing-request`) must be closed and dropped promptly upon completion or failure.
   - Leak detection assertions monitor host descriptor tables before and after execution.
2. **Layered Error Handling & Trap Safety:**
   - **Component Task Boundary:** Unhandled runtime rejections or host failures (e.g. `ErrorCode::ConnectionRefused`) surface as ECMAScript exceptions or idiomatic WIT `result<T, E>`. If an unrecoverable invariant is violated, a WebAssembly trap carrying diagnostic context occurs at the sandbox boundary, leaving the host process intact.
   - **CLI Command Boundary:** Exit codes (`wasi:cli/exit@0.2.x`) and standard error diagnostics (`wasi:cli/stderr@0.2.x`) are layered *only* on top within the CLI command adapter (`wasi:cli/run@0.2.x`). Pure task components remain decoupled from CLI exit semantics and reusable in embedded environments.
3. **Non-Blocking Polling:**
   - Concurrent async operations (e.g. `Promise.all([fetch(...), fetch(...)])`) must dispatch in flight simultaneously and await completion using `wasi:io/poll.poll` rather than sequential blocking or busy-waiting.

---

## 4. Component Task Model (Exported Functions)

`perry-wit` does not invent a proprietary task framework, runtime decorator, or custom DSL. Instead, it directly maps **standard TypeScript ES module exports** (`export function ...`) 1:1 to **standard WebAssembly Component Model exported functions** (`export func(...)`).

Any arbitrary TypeScript module exporting functions can become a component task accepting and returning structured types across the Canonical ABI boundary without passing CLI strings or invoking `_start`.

### Illustrative Example: Typed HTTP Worker

#### Standard TypeScript Source (`task.ts`)
```typescript
// Standard TypeScript syntax — zero Perry-specific imports or decorators:
export interface FetchRequest {
  url: string;
  timeoutMs?: number;
}

export interface FetchResponse {
  status: number;
  data: string;
}

export async function handleRequest(req: FetchRequest): Promise<FetchResponse> {
  const res = await fetch(req.url);
  const data = await res.text();
  return { status: 200, data };
}
```

#### Corresponding Component Model WIT (`world.wit`)
```wit
package example-host:http-worker@0.1.0;

interface types {
  record fetch-request {
    url: string,
    timeout-ms: option<u32>,
  }

  record fetch-response {
    status: u16,
    data: string,
  }
}

world http-worker {
  include wasi:http/outgoing-handler@0.2.6;
  include wasi:clocks/monotonic-clock@0.2.6;

  export handle-request: func(req: fetch-request) -> result<fetch-response, string>;
}
```

#### Canonical ABI Lowering
When compiling this module, `perry-wit`:
1. Identifies that `handleRequest` in TypeScript satisfies `export handle-request` declared in `world.wit`.
2. Generates the Canonical ABI entrypoint (`$cabi_handle_request`), unpacking incoming record arguments from host-allocated linear memory into Perry's internal NaN-boxed objects.
3. Invokes the TypeScript function.
4. Lowers the returned JavaScript object into the Canonical ABI memory layout (`result<fetch-response, string>`) and returns it to the host caller.
5. Emits the corresponding `$cabi_post_handle_request` cleanup hook to release temporary arena memory once the host has consumed the result.

This enables any standard TypeScript function to be orchestrated as an isolated, typed task within host engines.

---

## 5. Linking & Distribution Models

`perry-wit` supports two distinct distribution modes:

| Dimension | Standalone Component (`--target=standalone`) | Modular Task Component (`--target=modular`) |
| :--- | :--- | :--- |
| **Packaging** | Bundles application logic + guest runtime into a single WASIp2 component. | Emits lightweight task component (~5–10 KB) importing runtime interfaces. |
| **Dependencies** | Self-contained. Runs on any WASIp2 engine (`wasmtime run`). | Host-bound or dynamically linked at deployment. |
| **Footprint** | ~118 KB (stripped). | ~5–12 KB. |
| **Use Case** | CLI distribution, independent binary execution, standalone microservices. | High-density task orchestration (`example-host`), FaaS pipelines, dynamic workflows. |

At runtime in `example-host`, modular components link dynamically:
- Either via native Rust host providers satisfying the `perry:runtime` WIT world directly in host memory.
- Or via `wac plug task.wasm --plug guest-runtime.wasm` during deployment.

---

## 6. Zero-Config Consumer Authoring Architecture & Type Synchronization

To give component authors an ergonomic workflow where they can simply run `nix develop` and immediately start writing type-safe code, `perry-wit` implements an AST-based type generator and automated Nix integration:

```
  ┌────────────────────────────────────────────────────────┐
  │                 WIT World Definition                   │
  │                  (wit/world.wit)                       │
  └───────────────────────────┬────────────────────────────┘
                              │
                    [1. Pure Rust AST Parser]
                    (wit-parser / resolve_wit)
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │                SDK Code Generator (AST)                │
  │                 (src/sdk/codegen.rs)                   │
  └───────────────────────────┬────────────────────────────┘
                              │
               [2. TypeScript Declarations (.d.ts)]
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │              .perry/types/world.d.ts                   │
  │   - Interface definitions for WIT records              │
  │   - Discriminated unions for WIT variants/enums        │
  │   - Strongly-typed exported function declarations      │
  │   - Namespaced imported host interfaces                │
  └───────────────────────────┬────────────────────────────┘
                              │
               [3. DevShell / CI Synchronization]
                              │
  ┌───────────────────────────▼────────────────────────────┐
  │       IDE Type Checking (tsc) & Hermetic Sandbox       │
  │   - tsconfig.json automatically maps typeRoots         │
  │   - nix flake check runs sdkSyncCheck                  │
  │   - lib.buildComponent compiles hermetically           │
  └────────────────────────────────────────────────────────┘
```

### Type Lowering Rules

The pure Rust AST generator lowers WIT types into TypeScript according to strict semantic mapping:
- **Primitives:** `string`, `bool`, `u8`..`u32`, `s8`..`s32`, `f32`, `f64` map directly to `string`, `boolean`, and `number`. Large integers `u64` and `s64` map to `bigint`.
- **Records:** Lower to `export interface <Name> { ... }`.
- **Variants:** Lower to discriminated unions `export type <Name> = { tag: "foo", value: ... } | ...`.
- **Enums:** Lower to string union literals `export type <Name> = "foo" | "bar"`.
- **Options and Lists:** Lower to `T | null | undefined` and `Array<T>` (or `Uint8Array` for `list<u8>`).
- **Exported Functions:** Generate top-level function declarations (`export declare function foo(...): ...`) and companion type aliases (`export type FooFn = (...) => ...`).

### Hermetic CI Verification

To guarantee that component implementations never desynchronize from their declared WIT contracts:
- `checks.sdkSyncCheck`: Regenerates `.perry/types/world.d.ts` in an isolated Nix sandbox and executes `tsc --noEmit`. Any mismatch or missing export fails CI immediately.
- `lib.buildComponent`: Provides a declarative Nix builder that automatically coordinates WIT resolution, TypeScript compilation, and component wrapping in consumer flakes.

---

## 7. Reusable Concepts & Future Capability Expansions

To expand the capabilities of `perry-wit` while maximizing code reuse across the stack:

1. **Unified Dual-Sided Host/Guest Bindings:**
   - *Current State:* Guest declarations are generated via `perry-wit gen-types`, while host runners manually bind components or use `wasmtime::component::bindgen!`.
   - *Expansion:* Provide a unified CLI and library module that emits both the TypeScript guest contract (`.d.ts`) and the Rust host adapter structs from the same WIT package. This eliminates contract divergence between host runtimes (like `example-host`) and guest components.

2. **Async Component Model (`cm-async`) & Stream Lowering:**
   - *Current State:* Task functions are synchronous or block synchronously on WASI HTTP polling.
   - *Expansion:* Lower JavaScript `Promise<T>` and `AsyncIterable<T>` into native Component Model `future<T>` and `stream<T>`. This allows component tasks to pipe I/O streams directly between components without buffering large payloads in guest memory.

3. **Modular Capability Slices:**
   - *Current State:* The guest runtime adapter bundles standard I/O and HTTP into `guest_runtime.wasm`.
   - *Expansion:* Split runtime capabilities into composable feature slices (`perry:http`, `perry:kv`, `perry:blob`). Consumer components declare only the capability slices they need in their `world.wit`, and `buildComponent` links only the required runtime slices, reducing component size to 5–15 KB.

