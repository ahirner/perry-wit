# Perry-WIT Roadmap: Runtime Foundations & WASI Preview 2 Capabilities

Deliver useful TypeScript APIs in small slices with observable acceptance outcomes.
Phase numbers identify capability areas rather than a fixed implementation sequence.

## Choosing the Next Slice

1. Slices **C.2**, **7.1**, **9.1**, and **9.3** are closed with complete test coverage, indirect-call fixtures, option validation, and recorded sizes.
2. HTTP metadata/methods (**10.1**) and buffered binary bodies (**10.2**) are complete. Use those client paths and fixtures as the baseline for handlers and streaming.
3. One-shot timers and intervals (**11.1**, **11.2**) establish callback execution (**B.1**) and retained lifetimes (**E.2**), including reclamation between callbacks. Develop guest async execution (**B.2**) around an awaited timer or handler, extending these paths where the consumer needs it.
4. Expand into streaming and TCP after a buffered or one-shot use case works. Host adapters (**A.1**) and Component Model async (**13.1**) follow concrete integration needs.

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

## Verification Baseline (2026-10-03)

- `nix develop -c cargo test --locked --package perry-wit`: 147 tests passed, including 25 filesystem tests and 12 HTTP regression tests. HTTP and conformance suites use local socket fixtures; verification used a fresh temporary directory and a writable Cargo target directory.
- The per-slice commands below select tests from that run. A passing suite only establishes the cases it contains; missing acceptance evidence remains unchecked.
- Scoped formatting passed. Project and WebAssembly guest Clippy checks completed with existing warnings. Automatic approval review rejected parent `make format-rs` because it could rewrite the broader workspace; parent `make lint-rs` fails because `monty-bench` is outside this workspace. The pinned, scoped Nix checks are the applicable checks here.
- SDK generation describes selected WIT contracts, not ambient JavaScript/Node API compatibility. These runtime slices do not change WIT export types; supported API subsets and limits belong in the capability catalog and tests.
- Catalog `conformance` references can identify TypeScript differential cases or Rust integration suites. The differential report executes only TypeScript cases; integration-only entries remain `MISSING` in that report and are verified separately by Cargo.

## Runtime Foundations

### Item A: Shared WIT Host/Guest Contracts

- [ ] **A.1. Host/Guest Contract Parity** — Take when a Rust host integration needs it.
    - [ ] Exercise the same resolved WIT package/world through guest declarations and Rust host bindings, starting with currently supported export types that integration uses.
    - [ ] Verify a host invocation agrees with the generated guest contract; use existing Wasmtime binding tools for ABI marshalling where they fit.

A custom host generator is deferred until an integration demonstrates missing reusable glue.

### Item B: Guest Callbacks & Async Execution

`JsHandle::Closure` retains raw captured values, and the runtime creates and invokes synchronous guest function values.
The linker generates typed callback calls into TypeScript table 0 while Rust indirect calls remain in table 1.
Read-only lexical captures can retain mutable objects/views; shared mutable bindings and other unsupported function forms currently produce diagnostics.
The pinned Perry backend can emit named async functions as unresolved `rt:__async_<name>` imports, while trampoline mapping excludes async functions.
Existing HTTP polling does not establish general guest async execution.

- [x] **B.1. Guest Callback Execution** — Develop retained capture lifetimes with E.2.
    - [x] Make guest function values callable through a path that respects the linker's TypeScript/Rust table separation; select the bridge to match the emitted callback ABI.
    - [x] Support the captures needed by the first consumer, including their lifetime beyond the creating call; diagnose unsupported closure forms.
    - [x] Verify an identity callback returns its argument, a captured value survives delayed invocation, and repeated creation/invocation/release has bounded memory. Cover shared mutable captures if they are exposed.

Verification:

- `runtime_regression_test` compares callback values, named aliases, missing/excess parameters, evaluation order, nested invocation, and void returns with Node; it also tests exception propagation and unsupported-form diagnostics.
- `retained_callback_graphs_survive_calls_and_release_on_success_and_failure` exercises 1,000 retained/released callback cycles after warm-up, including captured strings/views, object/closure cycles, recoverable throws, and skipped post-return; linear memory stays at its warmed high-water mark. The same callback path runs through a Wasmtime component export.
- The first scheduled consumer is 11.1's one-shot timer. Its callbacks retain strings, cyclic objects, and byte views after the creating function returns, then release ownership on delivery or cancellation. Shared mutable lexical bindings and the other diagnosed function forms remain outside the supported callback subset.

- [ ] **B.2. Guest Async Execution** — Needs retained suspended state from E.2; use B.1 where the chosen lowering invokes guest callbacks.
    - [ ] Reproduce the named async export failure with a minimal `async runTask`, then make its body compile, link, and execute in the guest. Choose a backend change or upgrade based on that probe.
    - [ ] Support suspension, resumption, and rejection for the first task's `async`/`await` subset, reusing existing HTTP polling where practical.
    - [ ] Make exported async tasks complete through the host ABI with the declared result/error behavior; a synchronous Preview 2 boundary may drive the guest operation to completion.
    - [ ] Verify named exports, nested awaits, a genuinely pending operation, rejection propagation, and release of completed/rejected state through component invocation.

Start with scalar/string tasks; richer async signatures follow demonstrated consumers.
Component Model futures/streams remain a separate experiment in Phase 13.

### Item C: Safe Runtime Pruning

Pure operations use `mem_call_pure`; the linker selects specialized dispatch for all 16 combinations of clocks, randomness, environment, and filesystem access.
HTTP uses the broader dispatcher; minimal imports for mixed HTTP capability combinations have not been established.

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

- `nix develop -c cargo test --test pruning_test --test linker_test --test abi_regression_test --test runtime_memory_test --test http_regression_test`: all test suites pass cleanly.
- `test_merge_core_modules` in `tests/linker_test.rs` compiles TypeScript in-process via `perry_wit::compiler::compile_typescript_raw` and executes without skipping.
- Dedicated indirect table fixture `table_indirect_calls_preserve_runtime_functions_and_imports` in `tests/linker_test.rs` validates indirect `call_indirect` function table dispatch across merged modules.
- `exported_json_task_prunes_http_and_runs_without_http_bindings` in `tests/pruning_test.rs` proves pure JSON tasks completely prune `wasi:http`, `wasi:clocks`, `wasi:filesystem`, and `wasi:random` from the component and run successfully under wasmtime without `-S http=y` or `-S inherit-network=y`.
- `http_task_retains_http_and_verifies_abi_marshalling_and_cleanup` in `tests/pruning_test.rs` validates `cabi_realloc` host allocation, `result<string, string>` return marshalling, `cabi_post_run-task` cleanup, retained `wasi:http` imports in WAT, and live HTTP request execution against a local HTTP test fixture.
- Component and core wasm sizes recorded at C.2 closure (`record_pruning_and_component_sizes` in `tests/pruning_test.rs`):
  - Pure JSON Task: raw TS wasm: 9,787 B; guest runtime: 255,991 B; unlinked total: 265,778 B; pruned merged core: 172,400 B (64.9% of unlinked, 93,378 B pruned); final stripped component: 173,533 B.
  - HTTP Task: raw TS wasm: 9,809 B; guest runtime: 255,991 B; unlinked total: 265,800 B; pruned merged core: 215,280 B (81.0% of unlinked, 50,520 B pruned); final stripped component: 226,883 B.
  - The pure task stripped component is 53,350 B smaller than the HTTP task component, confirming dead HTTP runtime logic is pruned.

### Item D: Shared Binary Values

`JsHandle::Uint8Array` holds views with shared byte storage, offsets, and lengths.
Construction, indexed access, and subarrays serve random fills and filesystem byte I/O.

- [x] **D.1. Byte Storage & Uint8Array Views** — Build with the first binary API; reuse instances only with E.1's lifetime guarantees.
    - [x] Support byte storage and views with identity, offset, length, and shared mutation; choose a representation compatible with existing guest values and their ownership.
    - [x] Implement the construction, indexed access, and subview behavior needed by that API, keeping bytes intact across its host boundary.
    - [x] Verify mutation through overlapping views, bounds, empty views, non-UTF-8 bytes, and cleanup for the supported lifecycle.

Verification:

- `nix develop -c cargo test --test binary_views_test --test random_test --test fs_test`: 14 byte-view, 8 randomness, and 25 filesystem tests passed in the baseline run.
- Tests cover byte coercion, copy construction, invalid lengths/indices, shared subviews, JSON object shape, exact filesystem bytes, and random-fill identity/quota handling. The supported surface is `Uint8Array`; further view types and Node `Buffer` compatibility follow specific consumers.
- Repeated byte I/O is exercised within an invocation. Suspended or callback-owned views need the retained-lifetime coverage in E.2.

### Item E: Value Lifetimes & Bounded Memory

`RuntimeState` now reclaims invocation temporaries and recycles string/handle slots while retaining initialization values, registered globals, and process-context handles.
Post-return hooks release ABI buffers and trigger value reclamation. Callback graphs and pending timer arguments are roots until their owner releases them; suspended async work will extend those rules with B.2.

- [x] **E.1. Repeated Task Calls**
    - [x] Establish which values outlive a call, including literals, globals, returned values, and pending work; use that evidence to choose a reclamation approach.
    - [x] Reclaim call temporaries once results have been consumed while preserving surviving values. Invocation-scoped storage is an option only where escape behavior permits it.
    - [x] Verify repeated allocating string/JSON calls in one instance, with post-return each time, reach bounded live allocations and a stable memory high-water mark after warm-up; include recoverable failures and retained globals.
    - [x] Define and test whether an instance remains reusable after failures that interrupt normal cleanup.

Verification:

- `nix develop -c cargo test --test repeated_task_calls_test --test runtime_memory_test`: 7 repeated-call and 2 ABI memory tests passed in the baseline run, including the callback graph and timer lifecycle tests.
- Coverage includes stable memory after warm-up, retained global arrays/strings, recoverable result failures, skipped post-return recovery, direct component invocation, and allocation/reallocation exhaustion trapping rather than returning address zero.
- Checkpoints, global scans, slot recycling, and return-area tracking are implemented in `state.rs`, `cabi.rs`, and ABI trampolines. Recovery claims cover the tested interrupted-cleanup paths; arbitrary traps or exhausted instances are not promised reusable.

- [x] **E.2. Retained Values for the First Async/Callback Consumer** — Deliver with B.1 or B.2, extending E.1's ownership rules.
    - [x] Keep that consumer's captures or suspended values alive until their owner completes or releases them; choose retention/reclamation around the actual value graph, including any supported cycles.
    - [x] Verify survival across calls or suspension, reclamation after release, and protection against stale handles aliasing newly allocated values.
    - [x] Verify repeated retained-work cycles stay bounded on success and failure; include cancellation if the consumer exposes it.

Verification:

- The retained callback graph test under B.1 verifies survival across component calls and reclamation after release. `scheduled_callbacks_release_captures_resources_and_stale_ids` extends that graph ownership to queued timers, including strings, cyclic objects/closures, shared byte views, and raw callback arguments.
- The timer test runs 100 warm-up and 1,000 further cycles with delivery, cancellation, caught/uncaught guest errors, and skipped post-return cleanup. Fixed-size success/error payloads keep memory at its warmed high-water mark; every created subscription is dropped and live pollables remain bounded. Repeated expired IDs and invalid IDs leave newly scheduled work intact. Initialization and synchronous failures also release pending subscriptions. Arbitrary host traps and exhausted instances are outside the recovery guarantee.

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

- `nix develop -c cargo test --test clocks_test`: 9 tests passed in the baseline run, covering clock bounds/precision, Node comparisons for constructor arguments and ISO formatting, invalid dates, and exception control flow.
- Current-time construction, numeric timestamps, copying Date values, and the tested null/undefined/boolean conversions are supported. Calendar getters use UTC. String parsing, multi-argument constructors, timezone behavior, and the rest of the Date API are outside this slice.

### Phase 7: Randomness (`wasi:random`)

- [x] **7.1. UUIDs & Math.random()** — Does not need guest binary views.
    - [x] Expose `crypto.randomUUID()` backed by secure host randomness and `Math.random()` with values in `[0, 1)` through the pinned random interfaces.
    - [x] Verify UUID format/version/variant and numeric bounds with controlled edge inputs; collision sampling is not a correctness guarantee.
- [x] **7.2. In-Place Random Fill** — Needs D.1's view behavior.
    - [x] Make `crypto.getRandomValues(view)` fill a supported integer view's byte range and return that exact view, with type/size validation; start with `Uint8Array` if sufficient.
    - [x] Verify mutation, return identity, alias visibility, nonzero offsets, unchanged bytes outside the view, empty views, and invalid arguments using controlled random input.

Verification:

- `nix develop -c cargo test --test random_test`: 8 tests passed in the baseline run; `tests/conformance/cases/09_random.ts` also passed the Node comparison in `conformance_test`.
- 7.1's APIs, sampled UUID/range checks, and controlled host-input edge tests (bounds 0.0, max float < 1.0, 53-bit mantissa bit-masking, RFC 4122 v4 version and variant bits on all-0x00 and all-0xFF inputs, exact vector matching, and short host buffer padding) are verified in `tests/random_test.rs`.
- 7.2 has controlled host-byte tests for quota/type validation before host calls, empty views, return identity, and fills restricted to subviews. `getRandomValues` supports `Uint8Array` with a 65,536-byte quota; other view types remain outside the subset.

### Phase 8: Environment & Arguments (`wasi:cli`)

- [x] **8.1. Process Context**
    - [x] Expose host-provided environment and arguments as `process.env` / `process.argv` / `process.cwd()`, initializing only when needed and retaining values for their documented lifetime.
    - [x] Specify argument mapping and guest mutation behavior for the initial subset, including any differences from Node's executable/script prefixes.
    - [x] Verify supplied/missing/empty variables, argument order, and repeated access without initializing unrelated capabilities.

Verification:

- `nix develop -c cargo test --test env_test`: 6 tests passed in the baseline run; `tests/conformance/cases/10_env.ts` also passed the Node comparison. Tests cover supplied/missing/empty values, argument order, aliases, deletion, string coercion, enumeration, and capability pruning.
- `process.env` is a lazily cached, guest-local mutable snapshot; writes do not update the host environment. `process.argv` preserves the host's WASI argument list without synthesizing Node executable/script prefixes. `process.cwd()` returns `initial-cwd`, falling back to `/` if absent.

### Phase 9: Sandboxed Filesystem (`wasi:filesystem`)

- [x] **9.1. UTF-8 File Reads/Writes**
    - [x] Support `readFileSync` / `writeFileSync` within host-provided preopens, with explicit path rules and confinement through descriptor-relative operations.
    - [x] Verify round-trips, short I/O handling, missing files, denied access, path escape attempts, and resource cleanup on success/failure.
    - [x] Validate read encodings/options before I/O; unsupported encodings currently fall through to binary reads instead of a diagnostic.
- [x] **9.2. Binary File Reads/Writes** — Builds directly on 9.1's descriptor preopen routing and D.1's `Uint8Array` view behavior.
    - [x] Support `fs.readFileSync(path)` (without encoding or with binary encoding) returning `Uint8Array`, and `fs.writeFileSync(path, uint8array)` streaming raw byte slices via 4096-byte chunked `blocking_write_and_flush`. Writing with `binary` encoding requires a byte view; strings support UTF-8 and reject `binary` before opening the file.
    - [x] Verify arbitrary-byte round-trips, subview writes (with non-zero byte offsets), and repeated-operation stream cleanup.
- [x] **9.3. Directory & Metadata Operations** — Builds on 9.1's `locate_preopen` resolution.
    - [x] Support `fs.readdirSync(path)` via `Descriptor::read_directory` stream collecting child entry names, and `fs.unlinkSync(path)` via `Descriptor::unlink_file_at`.
    - [x] Support `fs.mkdirSync(path)` via `Descriptor::create_directory_at` and `fs.rmdirSync(path)` via `Descriptor::remove_directory_at`. Supplied `mkdirSync` options are evaluated and rejected before creating a directory; permission modes and recursive creation remain unsupported.
    - [x] Support `fs.existsSync(path)` via `Descriptor::stat_at` (never throws on non-existent paths).
    - [x] Support `fs.statSync(path)` via `Descriptor::stat_at(PathFlags::SYMLINK_FOLLOW, ...)`, exposing `{ isFile(): boolean, isDirectory(): boolean, size: number, mtimeMs: number }`.
    - [x] Verify listing, metadata, and removal for that subset, including failure paths (`ENOENT`, `ENOTDIR`, `EISDIR`) and descriptor cleanup.
    - [x] Preserve evaluation and reject unsupported options for all directory/metadata calls (`readdirSync`, `statSync`, `unlinkSync`, `rmdirSync`, `mkdirSync`); arguments are evaluated in standard left-to-right order and unsupported options are rejected before preopen resolution or mutation.

Verification:

- `nix develop -c cargo test --test fs_test`: 25 tests passed in the baseline run. Coverage includes UTF-8 and arbitrary bytes, offset views, named/namespace imports, metadata, directory operations, path confinement, failure cases, 128 KiB transfers, repeated I/O, import pruning, binding-aware unlink calls, and option validation before I/O across all fs calls.
- Unsupported write modes and binary string encodings are rejected before opening files. Read options/encodings are validated before I/O: only UTF-8 and binary encodings and flag 'r' are permitted; unsupported options throw a TypeError before file opening or preopen check.
- Directory and metadata operations (`mkdirSync`, `readdirSync`, `statSync`, `unlinkSync`, `rmdirSync`) preserve left-to-right evaluation order and reject unsupported options before preopen checking or mutating the filesystem. Supported writes overwrite; append/exclusive modes and permission changes remain unsupported.
- UTF-8 reads reject invalid UTF-8. Metadata is limited to size, modification time, and file/directory predicates; permissions, ownership, recursive operations, and broader Node `Stats` behavior are not implemented.

Promise-based filesystem APIs need guest async execution and evidence that their underlying operations can make progress; they are a later slice.

### Phase 10: HTTP (`wasi:http`)

Existing GET/POST, string-body, header, and response status/ok support is covered by `tests/http_regression_test.rs`.
Use that baseline when extending the client.

- [x] **10.1. Client Metadata & Methods**
    - [x] Send outgoing headers and string request bodies through the existing HTTP path.
    - [x] Add the response header access and additional methods required by a client use case, with validation and documented status-text behavior supported by the host.
    - [x] Verify the new behavior alongside existing GET/POST, status, and error-response behavior using a controlled fixture.

Verification (10.1):

- The full `cargo test --locked --package perry-wit` suite passed 133 tests under `nix develop` with a fresh temporary directory. After adding diagnostic and error-status coverage, `cargo test --test http_regression_test` passed all 9 tests.
- Controlled fixtures verify method spelling, GET/POST bodies, GET/HEAD body rejection, forbidden/invalid methods, header lookup and identity, absent/empty/duplicate fields, metadata after body consumption, and 200/404/500 responses. Invalid header names and unsupported mutations/iteration report diagnostics.
- Response headers expose read-only `get` and `has`. `statusText` is empty because the pinned WASI interface does not provide a reason phrase. Redirect handling and broader Headers APIs remain outside this subset. SDK WIT declarations are unchanged; the capability catalog records these limits.
- Scoped formatting and compiler/guest Clippy checks pass with existing warnings. The parent Makefile checks remain inapplicable as described in the verification baseline.

- [x] **10.2. Buffered Binary Bodies** — Needs D.1.
    - [x] Preserve byte views through request-option objects and shallow copies, keeping identity, shared mutation, and retained references intact; adapt object storage where the binary bridge exposes value copying.
    - [x] Support binary request/response bodies through the shared byte representation and verify byte-exact round-trips and cleanup.

Verification (10.2 object-storage prerequisite):

- The full `cargo test --locked --package perry-wit` suite passed 136 tests under `nix develop`, including local HTTP fixtures. Compiler and guest Clippy checks pass with existing warnings; applicable scoped formatting passes.
- `tests/binary_views_test.rs` compares object/spread/assign identity, shared mutations, enumeration, key order, nested UTF-16 strings, and circular JSON errors against Node. `tests/repeated_task_calls_test.rs` verifies retained cyclic objects and nested byte views survive post-return, then release without memory growth after warm-up over 1,000 cycles.
- This prerequisite changes guest value storage; it does not change WIT SDK contracts or establish callback/suspended-value lifetimes for E.2.

Verification (10.2 binary transfer and cleanup):

- The full `cargo test --locked --package perry-wit` suite passed 139 tests under `nix develop`, including all 12 HTTP tests. Compiler and guest Clippy checks and scoped formatting pass with existing warnings; the parent Makefile limitations remain as recorded in the baseline.
- `tests/http_regression_test.rs` verifies exact request/response bytes, offset subviews passed through options/spreads, empty bodies, and a 131,073-byte transfer through live HTTP fixtures. Text/JSON decoding handles a UTF-8 BOM and invalid-byte replacement without changing cached bytes. Ordinary arrays and objects are rejected as binary bodies before dispatch; GET/HEAD body validation still applies.
- A resource-counting WASI host runs 600 calls in one instance, including unread futures/responses, retained nested responses, and skipped post-return cleanup, with stable memory after warm-up and bounded resources. A partial body-read failure releases its stream, body, and error resource; the next invocation releases the failed response and successfully transfers bytes again. The component invocation/pruning tests verify the cleanup ABI remains valid and pure tasks retain no HTTP imports.
- Request bodies accept strings or `Uint8Array`; `Response.bytes()` returns an independent view. Buffered reads can repeat from cached bytes. ArrayBuffer, Blob, FormData, cloning, single-use body semantics, and streaming remain outside this subset; unsupported body APIs report diagnostics. Host resource drops run at ordinary invocation boundaries because component post-return cannot call imports. SDK WIT declarations are unchanged; the capability catalog records these limits.

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

- [x] **11.1. One-Shot Timers** — Needs B.1, retained callback lifetimes, and monotonic clocks; promise-facing behavior also needs B.2.
    - [x] Deliver `setTimeout` callbacks and support `clearTimeout`, using clock pollables and a pending-work representation appropriate to the runtime.
    - [x] Define observable callback ordering and idle termination; release captures and subscriptions when timers fire or are cancelled.
    - [x] Verify delivery exactly once, delayed capture survival, cancellation before firing and from another callback, idle termination, and bounded repeated allocations.

Verification:

- `nix develop -c cargo test --locked --package perry-wit --test clocks_test --test repeated_task_calls_test`: all 12 clock/timer and 7 repeated-call tests pass in the full baseline run. A Node comparison checks callback order, delayed captures, once-only argument evaluation, and extra arguments; the controlled clock host checks deadline ties, delay coercion, nested scheduling, cancellation, idle termination, stale IDs, and bounded resources/memory. Timers also run through actual Wasmtime CLI and component exports.
- `timers.rs` owns pending work and subscriptions. Direct timer call specialization retains only monotonic-clock/poll imports for timer-only tasks; the pure and mixed-capability pruning suites remain green. Invocations drain pending callbacks before returning; the synchronous function computes its return value before that drain. Post-return never performs host polling or resource drops.
- The catalog records direct global calls, numeric IDs, Node-style whole-millisecond delays, and the existing supported callback forms. Timer function values report diagnostics; intervals are covered in 11.2. Promise timers, microtasks, Node Timeout objects, and broader timer API forms belong to subsequent consumers; WIT export types and SDK contracts are unchanged. Validation uses indexed array iteration; a heterogeneous inline `for…of` probe currently hangs and remains a separate compiler limitation.
- [x] **11.2. Intervals** — Builds on 11.1 when recurring work is needed.
    - [x] Add `setInterval` / `clearInterval` with defined rescheduling and cancellation behavior compatible with the supported timer subset.
    - [x] Verify repeated callbacks, clearing during execution, and bounded memory/resources over many cycles.

Verification (11.2):

- `nix develop -c cargo test --locked --package perry-wit --test clocks_test --test repeated_task_calls_test --test runtime_regression_test`: all 29 tests passed. The Node comparison covers captures, extra arguments evaluated once, self-cancellation, cancellation before delivery and from another callback, interchangeable clear APIs, validation, and shadowed names. Property increment/decrement and numeric remainder bridges restore the recurring callback's state updates; the existing callback comparison checks coercion, prefix/postfix results, and receiver evaluation once.
- The existing controlled-clock lifetime test performs 50,000 interval callbacks after a 100-callback warm-up and checks linear memory stays at its warmed high-water mark during delivery. It verifies cyclic captures/views, globals, fresh string/result return values, void exports, stale IDs, skipped post-return, 100 failure/recovery cycles, and subscription release. A late clock checks rescheduling from callback start without replaying missed ticks. Timer-only components still prune unrelated capability imports, and an interval task also runs through a real Wasmtime component invocation.
- The ABI driver reclaims completed callback allocations with no live TypeScript frames, rooting the enclosing raw result and globals alongside pending timer graphs. Runtime hooks own and release subscriptions at ordinary invocation boundaries; post-return makes no host calls. Uncleared intervals keep the invocation running. SDK WIT contracts are unchanged; the capability catalog records the timer subset and rescheduling behavior.
- The full `cargo test --locked --package perry-wit` suite passed all 147 tests under Nix, including local HTTP fixtures. Scoped formatting and compiler/guest Clippy checks passed with existing warnings; the parent Makefile limitations remain as recorded in the baseline.
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
