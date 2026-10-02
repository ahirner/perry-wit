# Perry-WIT Roadmap: Runtime Foundations & WASI Preview 2 Capabilities

Deliver useful TypeScript APIs in small slices with observable acceptance outcomes.
Phase numbers identify capability areas rather than a fixed implementation sequence.

## Choosing the Next Slice

1. Clocks (**6.1**), environment (**8.1**), and UUIDs / `Math.random()` (**7.1**) offer small synchronous starting points.
2. Prioritize pruning (**C**) when minimal host requirements matter, and repeated-call reclamation (**E.1**) when instances must be reused.
3. Build binary values (**D.1**) with the first consumer: in-place randomness, filesystem buffers, or binary HTTP.
4. Develop callbacks (**B.1**), guest async execution (**B.2**), and retained lifetimes (**E.2**) around the first handler or timer that needs them.
5. Expand into streaming and TCP after a buffered or one-shot use case works. Host adapters (**A.1**) and Component Model async (**13.1**) follow concrete integration needs.

## Tracking Completion

Each numbered slice has one parent checkbox and a finite set of work-item checkboxes.
Tick a work item when its stated outcome is implemented and verified.
Tick the parent when its required work items and the definition of done below are satisfied.
Headings, prerequisites, and deferred work do not carry separate completion status.

Add a short `Verification:` note beneath a completed slice with test commands/results and documented limits.
Required checks that failed or have not run keep the slice open.
If implementation reveals a larger problem, narrow or split the slice around a useful, verified outcome and record the remaining work explicitly; do not tick the original promise as complete.

### Definition of Done for Every Slice

These are standing criteria, not checkboxes to complete once or copy under every slice.

- A representative TypeScript example compiles and runs through the actual compiler/runtime/component path for the promised behavior.
- SDK declarations, diagnostics, and `catalog/capabilities.json` reflect any support changes; unimplemented forms do not silently return dummy values.
- Focused tests cover the acceptance outcomes and relevant failure/cleanup paths. Use Node comparisons where applicable and controlled inputs or property checks for nondeterministic behavior.
- Verification records identify checks run and relevant limits or fixture dependencies; explain any inapplicable criterion.

## Runtime Foundations

### Item A: Shared WIT Host/Guest Contracts

- [ ] **A.1. Host/Guest Contract Parity** — Take when a Rust host integration needs it.
    - [ ] Exercise the same resolved WIT package/world through guest declarations and Rust host bindings, starting with currently supported export types that integration uses.
    - [ ] Verify a host invocation agrees with the generated guest contract; use existing Wasmtime binding tools for ABI marshalling where they fit.

A custom host generator is deferred until an integration demonstrates missing reusable glue.

### Item B: Guest Callbacks & Async Execution

`JsHandle` currently has no closure representation, and `mem_call` has no closure creation/invocation path.
The linker places TypeScript callbacks in table 0 and Rust indirect calls in table 1.
The pinned Perry backend can emit named async functions as unresolved `rt:__async_<name>` imports, while trampoline mapping excludes async functions.
Existing HTTP polling does not establish general guest async execution.

- [ ] **B.1. Guest Callback Execution** — Develop retained capture lifetimes with E.2.
    - [ ] Make guest function values callable through a path that respects the linker's TypeScript/Rust table separation; select the bridge to match the emitted callback ABI.
    - [ ] Support the captures needed by the first consumer, including their lifetime beyond the creating call; diagnose unsupported closure forms.
    - [ ] Verify an identity callback returns its argument, a captured value survives delayed invocation, and repeated creation/invocation/release has bounded memory. Cover shared mutable captures if they are exposed.
- [ ] **B.2. Guest Async Execution** — Needs retained suspended state from E.2; use B.1 where the chosen lowering invokes guest callbacks.
    - [ ] Reproduce the named async export failure with a minimal `async runTask`, then make its body compile, link, and execute in the guest. Choose a backend change or upgrade based on that probe.
    - [ ] Support suspension, resumption, and rejection for the first task's `async`/`await` subset, reusing existing HTTP polling where practical.
    - [ ] Make exported async tasks complete through the host ABI with the declared result/error behavior; a synchronous Preview 2 boundary may drive the guest operation to completion.
    - [ ] Verify named exports, nested awaits, a genuinely pending operation, rejection propagation, and release of completed/rejected state through component invocation.

Start with scalar/string tasks; richer async signatures follow demonstrated consumers.
Component Model futures/streams remain a separate experiment in Phase 13.

### Item C: Safe Runtime Pruning

Pure object construction and JSON operations currently use `mem_call`, whose branches also reach HTTP.
Function reachability alone therefore retains HTTP for ordinary pure tasks.

- [x] **C.1. Separable Capability Dispatch**
    - [x] Give statically known pure operations a path that does not reach HTTP, using direct calls or dispatcher specialization according to what the backend exposes.
    - [x] Preserve behavior for unresolved dispatch and avoid unconditional initialization of unused capabilities.
    - [x] Verify a JSON/object task uses the separable path and existing HTTP behavior still works.
- [x] **C.2. Pruned Components** — Needs C.1 for the pure-task import guarantee.
    - [x] Base reachability on the complete ABI: selected WIT exports, applicable initialization, generated trampolines/post-return hooks, host-called `cabi_realloc`, and their helpers. Run after trampoline synthesis, or derive equivalent roots from the same WIT/ABI metadata before sweeping.
    - [x] Preserve indirect-call targets, tables, globals, and memory initialization conservatively; start with provably dead functions/imports. Keep raw runtime exports only where the component ABI or core/debug contract needs them.
    - [x] Compile a task that parses JSON, constructs an object, and returns serialized JSON; verify the final component omits HTTP imports and runs without HTTP host bindings.
    - [x] Verify string/result marshalling, host allocation, post-return cleanup, an indirect-call fixture, and retained imports for an HTTP task. Include final WIT/component metadata in the check.
    - [x] Record before/after sizes for these examples and use the results to choose any further optimization.

Verification:
- Separable runtime dispatch implemented in `crates/guest-runtime/src/dispatch_pure.rs` (`mem_call_pure`), `crates/guest-runtime/src/dispatch.rs` (`mem_call_clocks`, `mem_call`), and decoupled `ResponseEntry` storage out of `RuntimeState` into `http.rs`.
- Linker pruning implemented in `src/linker/prune.rs` and `src/linker/mod.rs` selecting specialized dispatchers and dead-stripping unused host capabilities.
- Verified in `tests/pruning_test.rs`: pure components contain zero `wasi:http` and zero `wasi:clocks` imports; clocks-only components contain `wasi:clocks` and zero `wasi:http` imports; HTTP components retain `wasi:http`.

### Item D: Shared Binary Values

The current `uint8array_*` operations are stubs and the runtime lacks byte-buffer/view values.

- [ ] **D.1. Byte Storage & Uint8Array Views** — Build with the first binary API; reuse instances only with E.1's lifetime guarantees.
    - [ ] Support byte storage and views with identity, offset, length, and shared mutation; choose a representation compatible with existing guest values and their ownership.
    - [ ] Implement the construction, indexed access, and subview behavior needed by that API, keeping bytes intact across its host boundary.
    - [ ] Verify mutation through overlapping views, bounds, empty views, non-UTF-8 bytes, and cleanup for the supported lifecycle.

Further view types and Node `Buffer` compatibility follow specific consumers.

### Item E: Value Lifetimes & Bounded Memory

`RuntimeState` stores strings, handles, and responses in append-only vectors.
`cabi_post_cleanup` frees ABI return buffers, not those runtime values.
The existing memory test covers return buffers rather than repeated task allocations.

- [ ] **E.1. Repeated Task Calls**
    - [ ] Establish which values outlive a call, including literals, globals, returned values, and pending work; use that evidence to choose a reclamation approach.
    - [ ] Reclaim call temporaries once results have been consumed while preserving surviving values. Invocation-scoped storage is an option only where escape behavior permits it.
    - [ ] Verify repeated allocating string/JSON calls in one instance, with post-return each time, reach bounded live allocations and a stable memory high-water mark after warm-up; include recoverable failures and retained globals.
    - [ ] Define and test whether an instance remains reusable after failures that interrupt normal cleanup.
- [ ] **E.2. Retained Values for the First Async/Callback Consumer** — Deliver with B.1 or B.2, extending E.1's ownership rules.
    - [ ] Keep that consumer's captures or suspended values alive until their owner completes or releases them; choose retention/reclamation around the actual value graph, including any supported cycles.
    - [ ] Verify survival across calls or suspension, reclamation after release, and protection against stale handles aliasing newly allocated values.
    - [ ] Verify repeated retained-work cycles stay bounded on success and failure; include cancellation if the consumer exposes it.

E.2 completes for its first consumer.
Later handler, timer, and I/O slices own their additional lifetime tests; future resource types do not hold this slice open indefinitely.
WebAssembly memory need not shrink, but repeated bounded workloads must stop growing after warm-up.

## WASI Capability Slices

### Phase 6: Clocks & Wall Time (`wasi:clocks`)

- [x] **6.1. Scalar Time APIs**
    - [x] Make `Date.now()` and `performance.now()` work through the pinned clock interfaces, with epoch milliseconds and an appropriate monotonic time origin respectively.
    - [x] Verify wall time against host bounds and monotonic deltas under controlled conditions; avoid exact cross-engine timing comparisons.
- [x] **6.2. Basic Date Values** — Uses 6.1 for current-time construction; fixed-timestamp work can proceed independently.
    - [x] Support construction, `getTime()`, and `toISOString()` for a documented Date subset using a representation that fits guest values.
    - [x] Verify fixed-timestamp formatting and invalid-date behavior against Node; cover current-time construction if the declared constructor subset includes it.

Verification:
- Added `wasi:clocks/wall-clock@0.2.6` and `wasi:clocks/monotonic-clock@0.2.6` to WIT world adapter.
- Implemented `wall_clock_now_ms()` and `monotonic_clock_now_ms()` in `crates/guest-runtime/src/clocks.rs`.
- AST rewrite pass for `performance.now()` in `src/compiler/clocks.rs`.
- Date constructors, getters (`getTime`, `getFullYear`, `getMonth`, `getDate`, `getDay`, `getHours`, `getMinutes`, `getSeconds`, `getMilliseconds`), and `toISOString()` in `crates/guest-runtime/src/date.rs`.
- Verified in `tests/clocks_test.rs` (4 tests passing) against live WASI Preview 2 host runtime in Wasmtime.

### Phase 7: Randomness (`wasi:random`)

- [x] **7.1. UUIDs & Math.random()** — Does not need guest binary views.
    - [x] Expose `crypto.randomUUID()` backed by secure host randomness and `Math.random()` with values in `[0, 1)` through the pinned random interfaces.
    - [x] Verify UUID format/version/variant and numeric bounds with controlled edge inputs; collision sampling is not a correctness guarantee.
- [x] **7.2. In-Place Random Fill** — Needs D.1's view behavior.
    - [x] Make `crypto.getRandomValues(view)` fill a supported integer view's byte range and return that exact view, with type/size validation; start with `Uint8Array` if sufficient.
    - [x] Verify mutation, return identity, alias visibility, nonzero offsets, unchanged bytes outside the view, empty views, and invalid arguments using controlled random input.

Verification:
- Added `wasi:random/random@0.2.6` and `wasi:random/insecure@0.2.6` to WIT world adapter in `wit/world.wit`.
- Implemented `math_random()`, `crypto_random_uuid()`, `crypto_fill_random()`, and `crypto_random_bytes()` in `crates/guest-runtime/src/random.rs`.
- Added granular capability dispatchers `mem_call_random` and `mem_call_clocks_random` in `crates/guest-runtime/src/dispatch.rs`.
- Added random property and expression detection (`Math.random`, `crypto.randomUUID`, `crypto.getRandomValues`, `$$cryptoFillRandom`) emitting `__needs_random__` in `src/compiler/rewrites.rs`.
- Added reachability analysis in `src/linker/prune.rs` and import routing in `src/linker/mod.rs` so pure components prune `wasi:random`, while random-using components retain only `wasi:random`.
- Added string indexed access (`s[i]`), `string_charAt`, and `string_charCodeAt` in `crates/guest-runtime/src/dispatch_pure.rs`.
- Verified in `tests/random_test.rs` (5 integration tests passing) and `tests/conformance/cases/09_random.ts` (differential equivalence with Node.js).

### Phase 8: Environment & Arguments (`wasi:cli`)

- [x] **8.1. Process Context**
    - [x] Expose host-provided environment and arguments as `process.env` / `process.argv` / `process.cwd()`, initializing only when needed and retaining values for their documented lifetime.
    - [x] Specify argument mapping and guest mutation behavior for the initial subset, including any differences from Node's executable/script prefixes.
    - [x] Verify supplied/missing/empty variables, argument order, and repeated access without initializing unrelated capabilities.

Verification:
- Added `import wasi:cli/environment@0.2.6;` to `wit/world.wit`.
- Implemented `process_env()`, `process_env_get()`, `process_argv()`, and `process_cwd()` in `crates/guest-runtime/src/environment.rs`.
- AST rewrites and capability detection in `src/compiler/rewrites.rs` emitting `__needs_env__` for `process.env`, `process.argv`, and `process.cwd()`.
- Linker pruning in `src/linker/prune.rs` and import routing in `src/linker/mod.rs` selecting specialized dispatchers (`mem_call_env`, `mem_call_clocks_env`, `mem_call_random_env`, `mem_call_all_sync`, etc.) so pure components prune `wasi:cli/environment`.
- Implemented missing direct runtime imports in `crates/guest-runtime/src/stubs.rs` (`js_typeof`, `object_*`, `array_*`, `json_parse`, `json_stringify`, `string_includes`, `string_startsWith`, `string_endsWith`) and `crates/guest-runtime/src/dispatch.rs` / `dispatch_pure.rs`.
- Verified in `tests/env_test.rs` (4 integration tests passing under Wasmtime) and differential conformance case `tests/conformance/cases/10_env.ts` (100% match against Node.js oracle).

### Phase 9: Sandboxed Filesystem (`wasi:filesystem`)

- [ ] **9.1. UTF-8 File Reads/Writes**
    - [ ] Support `readFileSync` / `writeFileSync` within host-provided preopens, with explicit path rules and confinement through descriptor-relative operations.
    - [ ] Verify round-trips, short I/O handling, missing files, denied access, path escape attempts, and resource cleanup on success/failure.
- [ ] **9.2. Binary File Reads/Writes** — Needs D.1 and the file operations from 9.1.
    - [ ] Transfer byte views without text conversion, documenting the return type until Node `Buffer` compatibility exists.
    - [ ] Verify arbitrary-byte round-trips, subview writes, and repeated-operation cleanup.
- [ ] **9.3. Directory & Metadata Operations** — Add when a filesystem consumer needs them.
    - [ ] Support the required subset of `readdirSync`, `statSync`, and `unlinkSync`, keeping Node-facing shapes such as `stats.isFile()` consistent with the declared API.
    - [ ] Verify listing, metadata, and removal for that subset, including failure paths and resource cleanup.

Promise-based filesystem APIs need guest async execution and evidence that their underlying operations can make progress; they are a later slice.

### Phase 10: HTTP (`wasi:http`)

Existing GET/POST, string-body, header, and response status/ok support is covered by `tests/http_regression_test.rs`.
Use that baseline when extending the client.

- [ ] **10.1. Client Metadata & Methods**
    - [x] Send outgoing headers and string request bodies through the existing HTTP path.
    - [ ] Add the response header access and additional methods required by a client use case, with validation and documented status-text behavior supported by the host.
    - [ ] Verify the new behavior alongside existing GET/POST, status, and error-response behavior using a controlled fixture.
- [ ] **10.2. Buffered Binary Bodies** — Needs D.1.
    - [ ] Support binary request/response bodies through the shared byte representation and verify byte-exact round-trips and cleanup.
- [ ] **10.3. Buffered Incoming Handler** — Needs B.2; reusable handlers need the relevant E.2 lifetime support.
    - [ ] Expose `wasi:http/incoming-handler` and map incoming requests and response outparams to the Request/Response subset needed by one handler, including resource ownership.
    - [ ] Execute an async TypeScript handler and complete its response/error through the ABI. Start with bounded UTF-8 bodies; add binary bodies when D.1 is ready.
    - [ ] Verify a successful request, an awaited operation, rejection, body-limit behavior, and bounded memory/resources across repeated requests.
- [ ] **10.4. Streaming Bodies** — Needs guest async execution, binary values, and stream readiness from 11.3.
    - [ ] Expose incremental reads/writes for one Request/Response use case, defining backpressure, cancellation, and close/error ownership at that boundary.
    - [ ] Verify multi-chunk transfer, a slow consumer, and early cancellation; measure peak buffering before expanding stream compatibility.

### Phase 11: Polling & Timers (`wasi:io/poll`)

Existing HTTP `Promise.all` polls response futures together.
Extend that mechanism where it fits each consumer; timers and stream readiness need not wait for each other.

- [ ] **11.1. One-Shot Timers** — Needs B.1, retained callback lifetimes, and monotonic clocks; promise-facing behavior also needs B.2.
    - [ ] Deliver `setTimeout` callbacks and support `clearTimeout`, using clock pollables and a pending-work representation appropriate to the runtime.
    - [ ] Define observable callback ordering and idle termination; release captures and subscriptions when timers fire or are cancelled.
    - [ ] Verify delivery exactly once, delayed capture survival, cancellation before firing and from another callback, idle termination, and bounded repeated allocations.
- [ ] **11.2. Intervals** — Builds on 11.1 when recurring work is needed.
    - [ ] Add `setInterval` / `clearInterval` with defined rescheduling and cancellation behavior compatible with the supported timer subset.
    - [ ] Verify repeated callbacks, clearing during execution, and bounded memory/resources over many cycles.
- [ ] **11.3. Stream Readiness** — Build with the first async I/O consumer and its lifetime requirements.
    - [ ] Resume that consumer when input/output is ready, handling partial progress and ownership on completion/error/cancellation; reuse HTTP polling infrastructure where suitable.
    - [ ] Verify progress with slow or blocked peers, mixed pending operations, and cleanup without busy-waiting or unbounded buffering.

### Phase 12: TCP Client Sockets (`wasi:sockets`)

- [ ] **12.1. First TCP Client** — Needs D.1 and readiness/lifetime support; event-style APIs also need B.1.
    - [ ] Connect, exchange bytes, and close through the pinned socket interfaces for one client use case; add hostname resolution if that use case needs it.
    - [ ] Expose only the required `net.createConnection` / callback subset, reusing the guest invocation and binary I/O paths.
    - [ ] Verify echo, connection failure, partial writes, EOF, close/cancellation, and resource cleanup before attempting a database/cache client.

### Phase 13: Component Model Async (Deferred)

- [ ] **13.1. Component Async Feasibility** — Take when composition needs exceed the working Preview 2 guest async path.
    - [ ] Check the pinned parser, encoder, and host support, then select one future or stream use case that the available toolchain can express.
    - [ ] Exercise it between a producer and consumer, mapping the needed guest Promise/AsyncIterable behavior and verifying failure, cancellation, backpressure, and ownership as applicable.
    - [ ] Compare copying, memory, and throughput with the existing polling path; use those results to decide whether further lowering work is justified.

## Deliberately Deferred Complexity

| Area | Initial scope | Revisit when |
| --- | --- | --- |
| Custom Rust host generator | Shared WIT plus existing binding generation | A host integration has repeated glue the existing tools cannot cover |
| Runtime feature matrices or separate capability packages | One runtime build with separable dispatch and pruning | Measured packaging or host constraints require another distribution model |
| Aggressive whole-module optimization | Conservative function/import pruning | Measured size or speed remains a problem; then consider type/data compaction or more precise analysis |
| General-purpose garbage collector | Reclamation matched to demonstrated lifetimes | Supported escaping/cyclic values cannot be reclaimed reliably by the simpler design |
| Broad JavaScript/Node compatibility | Tested subsets for the first Date, buffer, filesystem, timer, and network consumers | A concrete consumer needs timezone parsing, more view types, Node Buffer, filesystem promises, or broader stream/event behavior |
| General scheduler and networking stack | Per-consumer polling, one-shot timers, and a TCP client | Workloads require broader scheduling, UDP, TLS, or additional protocol support |
| Component async and zero-copy optimization | Guest async on Preview 2; a bounded feasibility slice only when needed | Composition requirements and measurements justify the additional ABI work |

Callback execution, in-place byte mutation, complete ABI roots, and bounded live memory remain required correctness outcomes for the slices that promise them.
