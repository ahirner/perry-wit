# perry-wit

`perry-wit` compiles TypeScript ahead-of-time directly into native WebAssembly (WASI Preview 2) components. Rather than bundling dynamic JavaScript interpreters (such as QuickJS or SpiderMonkey), it lowers TypeScript through Perry's compiler pipeline and fuses the resulting module with an in-process static linker.

For runtime boundaries, compilation pipeline details, and conformance specifications, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Environment

For developing the Perry-WIT compiler, enter the repository's default shell:

```bash
nix develop
```

This supplies the Rust toolchain, Node.js, TypeScript, Wasmtime, `wasm-tools`, and
WASI Preview 2 definitions via `$WASI_WIT_PATH` and WASI 0.3 definitions via
`$WASI_P3_WIT_PATH`. The latter are also available through `nix build .#wasi-p3-wit`;
the `wasi-p3-wit` flake check resolves their complete CLI and HTTP dependency graph.
Cargo uses the prepared guest
runtime through `$GUEST_RUNTIME_PATH`; use `cargo build`, `cargo run`, and
`cargo test` to work on the compiler.

For authoring TypeScript components, select the SDK shell:

```bash
nix develop .#sdk
```

The SDK shell provides the packaged `perry-wit` compiler, `tsc`, Wasmtime, and
`wasm-tools`, and generates contracts for the current project's `wit/` directory.
See [Authoring Components](#authoring-components) for the template workflow.

---

## Usage

### Build

Build components and tools using Nix:

```bash
# Build the perry-wit CLI
nix build .#perry-wit

# Build the guest runtime
nix build .#guest-runtime

# Build the example component
nix build .#example-merge-docs
```

Or build locally with Cargo:

```bash
cargo build --release
```

### Compile

From the compiler checkout, compile a TypeScript script to a WASI Preview 2 component:

```bash
cargo run -- examples/merge_docs.ts -o dist/my_component.wasm
```

Pass `--core-only` to output unlinked Core WebAssembly without component wrapping, or `--wit <PATH>` and `--world <NAME>` to specify custom WIT contracts.

### Run

Execute the generated component with Wasmtime:

```bash
wasmtime run -S http=y -S inherit-network=y dist/my_component.wasm
```

### Test

Run unit tests, end-to-end integration tests, and flake validation:

```bash
# Unit and linker tests
cargo test

# End-to-end HTTP and splatting test
./scripts/test_e2e.sh

# Flake build and format checks
nix flake check
```

---

## Authoring Components

From an empty component project directory, initialize the template using a
Perry-WIT flake reference. For example, with a compiler checkout:

```bash
perry_wit_source="git+file:///absolute/path/to/perry-wit"
nix flake init -t "$perry_wit_source"
```

Set `inputs.perry-wit.url` in the generated `flake.nix` to the same reference,
then run `nix develop`. A repository reference such as
`github:<ORG-TBD>/perry-wit` can also be used; substitute the chosen organization.
The template's `../` default resolves relative to its directory.
To override the configured source for a command, pass
`--override-input perry-wit "$perry_wit_source"` to `nix develop` or `nix build`.

The template's default shell selects Perry-WIT's `devShells.sdk`, so plain
`nix develop` enters the component-author environment in generated projects.

For an existing component project, enter the SDK shell directly from its directory:

```bash
nix develop "$perry_wit_source#sdk"
```

When entering the SDK shell:

- `perry-wit gen-types` runs automatically if a `wit/` directory is present, emitting `.perry/types/world.d.ts` and `.perry/types/implementation-check.ts`.
- A missing `tsconfig.json` is generated with the implementation check included. The template already includes these files in its configuration.
- `perry-wit`, `tsc`, `wasmtime`, and `wasm-tools` are placed directly in `$PATH`.

### Type Checking & Building

Validate static types and build the component:

```bash
# Validate TypeScript implementation matches the WIT contract
tsc --noEmit

# Compile directly with the CLI
perry-wit src/index.ts --wit wit --world task -o dist/my_task.wasm

# Or package hermetically inside Nix via lib.buildComponent
nix build
```

You can also run type generation standalone:

```bash
perry-wit gen-types --wit wit --world task -o .perry/types
```

Generation also writes `.perry/types/implementation-check.ts`, which binds the
WIT contract to `src/index.ts`. Select another implementation with
`--entry src/my-task.ts`. The generated default tsconfig includes this check;
an existing custom tsconfig must include it in its `files` or `include` list.
Interface members use prefixed implementation names, such as `apiRunTask` for
`api`'s `run-task`; `ComponentImplementation` in `world.d.ts` lists the exact names.

## Contributing

Develop inside `nix develop` to ensure matching toolchain versions across dependencies. Format all code with `cargo fmt --all` and ensure both `cargo test` and `nix flake check` pass cleanly before submitting changes.

The Rust `compile_typescript_waffle` API provides the WAFFLE/P3 migration path.
It accepts `declare function waitFor(milliseconds: number): Promise<void>` and
`declare function randomNumber(): number` for typed P3 clock and random operations.
`randomNumber()` uses the high 53 bits of a host random word to produce a number
in `[0, 1)`. Host failures trap; these operations have no WIT domain-error result.
Negative, nonfinite, and overflowing wait durations trap before calling the clock.
Named and namespace imports from `perry:clocks` (`waitFor`) and `perry:random`
(`randomNumber`) support import aliases and literal member names.
Include [types/p3.d.ts](types/p3.d.ts) when type-checking these source tasks.
The zero-argument builtin `Math.random()` uses the same random operation.
`performance.now()` returns the host monotonic clock in floating-point milliseconds;
`Date.now()` returns whole UTC epoch milliseconds from the signed P3 system clock.
Only the selected clock functions are imported, including when reads and waits share
one component. Date values support `new Date(epochMs)` with a statically known
number, `.getTime()`, and `.toISOString()`. Numeric construction clips fractional
milliseconds and preserves invalid-date behavior. Invalid dates return NaN from
`getTime()` and throw numeric code `1` from `toISOString()` through catch/finally.
Dates retain distinct identity through helpers, object properties, stored Promises,
and native suspension. Pure epoch/ISO computations import no WASI capabilities.
Omitted, copy, nonnumeric, and multi-argument constructors, calendar getters,
`valueOf()`, setters, and string parsing are diagnosed. Dates remain guest-internal;
use UTC ISO strings at component boundaries.
`Temporal.Instant` supports `from(string)`, `fromEpochMilliseconds(number)`,
`epochMilliseconds`, and `toString()`. Instants retain nanosecond precision;
`epochMilliseconds` floors toward negative infinity. `Temporal.PlainDateTime`
supports `from(string)`, ISO date/time fields, `add({days: number})`, and `toString()`.
It has no time zone: parsing ignores numeric offsets and rejects `Z`.
Both types are immutable, retain identity through records and stored Promises,
and survive native suspension and collection under a 256 KiB guest cap.
Parsing, range, and unsupported bracket annotations throw numeric `1`, `2`, and `3`.
Object overloads, other duration fields, options, timezone databases, and implicit
JSON serialization are unsupported; serialize explicitly with `toString()`.
The [UTC fixture](tests/fixtures/temporal_utc.ts) restricts interchange to
`YYYY-MM-DDTHH:mm:ss[.1–9 digits]Z`. The [day-shift fixture](tests/fixtures/temporal_shift.ts)
checks timezone-free calendar arithmetic and codec errors.
`waffle_backend::compile_typescript_for_world` accepts a resolved WIT world and
uses the generated SDK's implementation names. Synchronous and directly awaited
asynchronous WIT imports/exports support booleans, 8–32-bit integers, floats,
strings, records, fixed tuples, enums,
variants, results, nullable options, typed lists, and flags with at most 32 fields.
WIT `u64` values use lossless `bigint` transport; arithmetic, coercion, comparison,
and literal construction are unsupported. Signed 64-bit values remain unsupported.
Integer outputs and tuple/list indices are checked. Dense typed arrays support
construction, indexed replacement, and single-element `push`; string arrays use
separate immutable storage. Sparse and heterogeneous mutation is rejected.
Named and namespace WIT imports retain binding identity; generated SDK modules
expose matching functions and local type names. Small independent fixtures cover
[records and tuples](tests/fixtures/record_boundary.ts) and
[optional imports](tests/fixtures/optional_import.ts), including missing versus
empty values, variant errors, and shared-heap cleanup under bounded memory.
Runner is an external reference for requirements and optional smoke tests;
its source and WIT definitions are not vendored. Resolved worlds can use the
existing P3 HTTP, filesystem, stdio, context, clock, and random operations when
the world imports their WASI 0.3 interfaces. The standard component encoder
binds them to the shared guest memory. Calls are serial; retained tasks remain
unsupported on this path, and host traps or interruption require store disposal.
Nested options and guest resource APIs remain unsupported; the production CLI
still uses the legacy path.
Shared tagged values preserve strict equality, truthiness, type tags, typed object
fields, and retained `Promise<any>` outcomes. Typed boundaries validate the stored
kind and throw numeric `12` on a mismatch. Public `any` component parameters/results
retain their numeric contract; heterogeneous values remain guest-internal.
Awaiting or adopting a Promise hidden
inside `any` throws `12` until dynamic Promise outcome tags are supported. Callback
timers remain separate migration work.
`JSON.parse(string)` and compact `JSON.stringify(value)` use the Rust UTF-8 codec.
Parsed objects and mixed arrays share the guest value representation, preserving
aliases, nested mutation, helper calls, retained Promises, and collection. Plain
objects, dense array literals, string arrays, primitives, and Dates serialize with
ECMAScript number formatting and key ordering. Undefined object fields are omitted;
array holes and undefined elements become null; an undefined root returns undefined.
Invalid Dates serialize as null. Syntax, unpaired surrogate escapes, and excessive
depth/cycles throw numeric codes `1`, `2`, and `3`; unsupported value kinds throw `12`.
Objects accept string property keys. Mixed arrays accept numeric/canonical index
keys, numeric length changes, deletion, membership, and `Array.isArray`; array
methods and arbitrary extra properties remain outside this subset. Sparse array
literals are diagnosed before lowering loses their holes. Revivers, replacers,
indentation, custom prototypes, and byte-view/Stats/Promise serialization are
unsupported. JSON graphs remain guest-internal; component boundaries carry JSON
as strings. Pure JSON tasks import no host capabilities.
`process.env`, `process.argv`, and `process.cwd()` lazily read the selected functions from
`wasi:cli/environment@0.3.0`. They cache guest-local values for the instance lifetime,
including across component returns, collection, and native suspension. Arguments
are the host's string list without synthesized Node executable/script prefixes.
The working directory uses `/` when the host supplies no initial directory; a
supplied empty string remains empty. Host failures trap. Retained heap roots keep
cached values alive while post-return collection reclaims invocation temporaries.
Environment and argument snapshots are read-only. Assignment, deletion, mutating
methods, and bulk writes are compile errors, including through aliases, casts,
containers, and helper calls. Missing environment keys read as `undefined`.
`Object.keys`, string-valued `Object.values`, and string-key `in` inspect own
properties. Enumeration and JSON produce independent snapshots; copying a string
from the environment into an ordinary record does not make that record read-only.
Environment reads do not link the JSON codec unless the source also uses JSON.
`crypto.getRandomValues(view)` fills only the visible `Uint8Array` range and returns
the same view. Empty views make no host call. Invalid supported value kinds throw
numeric code `1`; views larger than 65,536 bytes throw `2`, before requesting randomness
or changing bytes. Ordinary literal array arguments preserve their element effects
before rejection. Other typed-array classes remain unsupported source forms.
The shared fill loop handles P3 short reads and frees each temporary native byte
allocation. A zero-length or oversized host result violates the P3 contract and traps,
as do host failures; traps bypass guest cleanup and require store disposal. Earlier
writes are not rolled back. `crypto.randomUUID()` uses fresh random bytes, sets the
v4 version and variant bits, and returns a lowercase 36-character UUID. UUID text and
filled byte views survive retained Promises, collection, and native suspension.
Binding resolution distinguishes local shadows from the builtin and imported functions.
Dynamic member names, capability function values, spread arguments, parameter
defaults, and class initialization currently produce diagnostics.
Unused declarations introduce no component imports; dependencies of all emitted
function bodies are retained conservatively.
An entry may own one `ByteStream` parameter, represented by a native `stream<u8>`.
Immediately awaited `readChunk(input)` reads up to 8192 bytes into a reusable
managed buffer; `byteAt(index)` accesses the current chunk and traps on invalid indices.
Immediately awaited `readInto(input, destination)` reads directly into a `Uint8Array`
view and returns the number of bytes written. It leaves the rest of the view and
bytes outside the view unchanged. An empty destination returns zero without
consuming input. Both read APIs invalidate the previous chunk.
A zero count from a nonempty read means EOF; a capability's separate
completion future must still be checked for recoverable errors. Source helper
functions borrow the input, and entry cleanup closes it after `finally`, including
early returns and numeric errors. Calls are serial: the pinned host queues
overlapping calls before entry, with an additional guest guard against reentry.
Traps and cancellation require store disposal. Stored async tasks, multiple inputs,
and returned streams remain unsupported for this input contract. The filesystem tests
compose a source scanner with a real P3 producer and verify native file forwarding
against independent bindings. Filesystem source writes are available through named,
namespace, and default imports from `fs` or `node:fs`. `writeFileSync(path, data,
options?)` overwrites a file with exact UTF-8 string bytes or the visible
`Uint8Array` range. It accepts case-insensitive `utf8`/`utf-8` encoding labels,
and `binary` for byte data, with the default or explicit `w` flag. Options may be
an encoding string, null, undefined, or a plain data object with `encoding` and
`flag` fields. Arguments and duplicate property values execute in source order;
unsupported options fail before opening or truncating a file. Option objects support
locals, aliases, helper calls, retained Promises, mutation, and deletion through static
or runtime string keys. Nested values remain traced, and dead cycles are reclaimed.
Getters, setters, methods, spreads, computed literal keys, and custom prototypes are
diagnosed before frontend lowering can discard their effects. Unknown fields reject
the operation even when their values are undefined; deleting them removes that rejection.
Paths normalize `.` and `..` and select the longest matching preopen on a path
component boundary. Relative paths require a relative or root preopen; NUL paths
and escapes fail. The host confines symlink resolution to the selected preopen.
Writes return void and may suspend for native transfers and their separate completion
future. Descriptors, stream ends, and futures close before returning, and buffers
remain rooted during sibling collection. Errors throw the one-based ordinal of the
WASI 0.3 filesystem error variant, including `1` (access), `12` (invalid options),
and `32` (broken pipe); host traps and cancellation require store disposal.
Already written bytes are not rolled back.
`readFileSync(path, options?)` returns an independent `Uint8Array` for omitted/null
encoding or the `binary` label, and a string for `utf8`/`utf-8`
labels (all case-insensitive). Plain data options support `encoding` and the
default or explicit `r` flag; unsupported options fail before opening a file.
The `binary` label selects bytes here; Node treats it as Latin-1 text.
Reads materialize the file with storage proportional to its size, share the native
read transfers, and await the producer's separate completion before returning.
Text reads preserve BOMs and NULs and count Unicode scalars; malformed or unfinished
UTF-8 throws filesystem error `9` instead of Node's replacement decoding.
Returned values, pending buffers, and stored string/byte task outcomes survive collection.
Runtime string encoding labels and reusable option objects return `string | Uint8Array`.
Inline literals can select a more precise result; SDK object overloads conservatively
return the union. These values support `length`, truthiness, strict equality, assignment, helper calls, retained Promises,
and `writeFileSync`. Use `typeof value === "string"` (or `"object"`) before indexing,
calling methods, or passing the value to a string-only or byte-only consumer.
Assignments invalidate guards; loops and exception paths preserve the tagged value.
At component boundaries the union is exported as `text-or-bytes`, a variant with
`text(string)` and `bytes(list<u8>)` cases, also supported inside numeric-error
`Result` returns. `statSync` provides `size`, `mtimeMs`, `isFile()`, and
`isDirectory()`; signed native timestamps convert to milliseconds, and absent
modification times return zero. `existsSync` returns false on filesystem errors.
`mkdirSync`, `unlinkSync`, and `rmdirSync` accept only omitted/undefined options;
preopen roots cannot be removed. `statSync` accepts null/undefined or plain data
options with `bigint: false` and `throwIfNoEntry: true`.
`readdirSync` materializes UTF-8 names, excludes dot entries, and awaits both the
entry stream and its separate completion future. Ordering is host-defined. It
accepts a UTF-8 label or plain data options with `encoding`, `recursive: false`,
and `withFileTypes: false`. Argument effects precede validation and I/O; duplicate
known fields use their last value. All operations share confined, normalized,
longest-prefix preopen resolution. Directory arrays and Stats values survive
collection, guest helper calls, and repeated awaits of retained Promises. Component
parameters and results use `list<string>` for arrays and an exported `stats` record
with `size: f64`, `mtime-ms: f64`, and a `stats-kind` enum. Its cases are `block-device`,
`character-device`, `directory`, `fifo`, `symbolic-link`, `regular-file`, `socket`, and
`other`. Both values also work in numeric-error `Result` returns; guest identity is
preserved within an invocation. Plain option objects remain guest-internal and use
nonrecursive structural types or simple interfaces without inheritance or methods.
Runtime `delete` is diagnosed for all receiver types; clear optional fields with
`undefined` or construct a new record.
Dictionary enumeration uses insertion order, including numeric-looking keys.
JavaScript's integer-key sorting is deferred under D2.
`perry:http` exports immediately awaited `get(scheme, authority, path, headers,
maxResponseBytes)`. Scheme is `http` or `https`; headers are a string-valued
dictionary. The caller supplies an integer body limit. The buffered response
exposes read-only `status`, `body`, and `headerCount` properties, plus
`headerName(index)` and `headerValue(index)`; duplicate headers and exact binary
values are preserved. Body and header byte views retain their storage after
helper returns and collection. Native resources close before a response returns,
and producer completion is checked independently of EOF. Numeric errors are `8`
for body overflow, `12` for invalid metadata/limits/indices, `100 +` the WASI HTTP
error discriminant, and `200 +` the header error discriminant. Non-2xx statuses
remain responses. Traps and interrupted calls require store disposal. HTTP calls
cannot yet be combined with stored async tasks. The independent `tests/fixtures/http_json.ts` example checks status,
Content-Type, strict UTF-8, and JSON validation under a 64 KiB response limit.

The Rust `compile_http_handler` API exports `wasi:http/handler@0.3.0` from a typed
`handle(request: Request): Response` or async `Promise<Response>` function. Import
these record types from `perry:http-handler/types` (declared in `types/p3.d.ts`).
Requests carry a method variant, optional scheme/authority/path, duplicate header
byte pairs, and bounded body bytes. Compiler options require explicit request and
response byte caps. Responses use status 200–599, header byte pairs, and a byte
body; 204/205/304 require an empty body. Direct awaits can use P3 capabilities.
The component serializes invocations through response completion, publishes the
response before sending its body, and retains guest storage until transmission
and the consumer's separate completion settle. Hosts must drive the P3 event loop
through that completion. Request/response overflow returns the corresponding WASI
body-size error; invalid response metadata and request producer failures return
`internal-error`. Source exceptions and interrupted calls require store disposal.
Request trailers are consumed and released; response trailers and source streams
are not exposed. Consumer failure after publication closes and releases the
response; no second response or retry is attempted. Stored tasks remain diagnosed.

`perry:stdio` exports immediately awaited `writeStdout(bytes)` and
`writeStderr(bytes)`. They write the visible `Uint8Array` range, including arbitrary
binary bytes, through shared native stream transfers and wait for the capability's
separate completion future. Partial writes and backpressure preserve the unwritten
suffix; empty views still check completion. `console.log(text)` writes one string
and a newline to stdout; `console.error(text)` and `console.warn(text)` use stderr.
These console calls return void and may suspend for host output. Formatting,
multiple arguments, non-string coercions, and storing byte-output Promises are
currently diagnosed. UTF-8, embedded NULs, and argument effects are preserved.
Output errors throw numeric codes `1` (I/O), `2` (invalid byte sequence), or `3`
(broken pipe), reaching catch/finally and numeric WIT error results. Previously
written bytes remain visible. Writable ends and completion futures close before
the call returns; buffers stay rooted through suspension and sibling collection.
Returning streams that outlive the invocation still needs separate ownership.
`Uint8Array` values support numeric lengths, literal element arrays,
copy construction, indexed reads/writes, `subarray`, `slice`, `length`, `byteLength`,
and `byteOffset`. Views have distinct identity and retain their shared backing
allocation; `slice` and copy construction make independent bytes. Invalid indices
read as undefined and ignore writes. Invalid lengths throw numeric payload `1`
through the current exception ABI; allocation exhaustion traps. String coercions
and ArrayBuffer overloads still require further lowering. Stored byte-valued
Promises retain view identity and backing storage. Component parameters and results
use `list<u8>`, preserve arbitrary bytes, and share canonical allocation and post-return cleanup with text results.
Byte entry results also work after stored primitive tasks settle. Byte-only tasks
import no host capability.
`TextDecoder` supports strict incremental UTF-8 decoding of byte views through
`decode(bytes, {stream: true})`, followed by `decode()` to finish. It retains split
scalars, copies pending bytes, preserves immutable text results, and implements
`ignoreBOM`, `encoding`, and `fatal`. UTF-8 labels accept ASCII case and surrounding
ASCII whitespace. Under the scalar text contract, `fatal` defaults to true;
other encodings and replacement mode throw numeric `1`, and malformed or unfinished
UTF-8 throws numeric `2` through catch/finally and WIT results. Decoder objects and
pending bytes survive collection and suspension. Source helpers can borrow or return
decoders internally; component exports and stored tasks cannot carry decoder objects.
Options accept plain literal objects, null, or undefined; argument effects and
duplicate properties retain source order. Other option objects, getters, spreads,
custom prototypes, computed option names, and non-string label coercions are diagnosed. Streaming error
recovery retains unread bytes according to the
[Encoding Standard](https://encoding.spec.whatwg.org/#dom-textdecoder-decode);
the pinned Node version discards those bytes. Tests cover this difference explicitly.
WAFFLE string storage belongs to a serial invocation. Canonical post-return
reclaims its arena after the host copies the result, including recoverable WIT
errors. Raw core callers must invoke the matching `cabi_post_<export>` with the
core return values after consuming the result and before the next invocation.
Traps and cancelled calls still require discarding the instance. Typed root frames
retain references across source calls and suspended native tasks. Loop backedges
trace live strings, interior views, Promise outcomes, observers, byte views, and
their backing storage, then reclaim and coalesce dead allocations. Storage is
bounded by live values and a fixed
number of reference slots per active source frame. Native input reads share this
managed memory, retaining the chunk buffer and destination views through suspension
and collection. Native output calls retain their source and completion buffers until
both channels finish. Returned streams and callback owners still need lifecycle support.
String `for…of` iteration evaluates its input once and yields complete Unicode
scalars, including separate combining marks. It supports nested `for`/`while`
loops, numeric updates, `break`/`continue`, and `finally` cleanup across P3 waits.
Custom iterator protocols and labeled loop exits remain unsupported.
`text.search(/pattern/u)` returns a scalar position suitable for indexing or
`slice`. Literal patterns support groups, alternation, repetition, character
classes, anchors, ASCII word boundaries, and the `u`/`s` flags. Pattern atoms
always follow the scalar contract, including without `u`. Constructors, stored
RegExp values, other flags, lookaround, backreferences, and Unicode property
escapes are diagnosed. Regex tables are compiled ahead of time; searches allocate
no guest memory. Oversized automata fail at compile time.
Stored Promises from named async functions and the supported async imports retain
number, boolean, string, or void outcomes and numeric rejections. Starting a task
runs it up to suspension; aliases preserve identity and repeated awaits reuse its
outcome. Typed Promise parameters allow named tasks to observe the same outcome
concurrently. These components require Wasmtime's `wasm_component_model_async_stackful`
and `wasm_component_model_threading` features alongside `wasm_component_model_async`
and `wasm_component_model_more_async_builtins`.
Promise records share invocation storage and have 32-byte payloads. One observer
consumes the native completion and wakes queued observers in registration order;
each additional concurrent pending observer has an 8-byte payload. Allocations
also carry a 32-byte tracing header and alignment padding. Completed unreferenced
records and observer queues are reclaimed at loop collection points.
Settlement and subsequent awaits of the retained outcome allocate no guest bytes.
Promise parameters cannot cross the public WIT boundary. Callbacks, constructors,
and combinators remain unsupported. Detached call statements are diagnosed; returning while a
started operation remains pending or its native completion is unconsumed traps
before result delivery. The host must discard that instance. Simultaneous entry
calls are rejected; native child tasks may overlap within their owning invocation.
An observer's rejection leaves the shared operation available to other observers.
Individual observer cancellation is not exposed. Cancelling an invocation means
disposing of its store, which releases host operations and native task state;
guest `finally` blocks do not run after disposal. Completed serial calls release
native task state and reclaim the arena after copying the result.
The default CLI and SDK still describe the legacy pipeline until the R9 cutover.
The Rust `compile_typescript_raw` API returns a `RawCompiled` value containing
`core`, `exported_functions`, and `functions` for legacy linker consumers.


Task export ABI support currently covers strings, numeric and boolean scalars,
void results, and `result<string, string>` results. WIT record parameters and
results are rejected with a compile-time diagnostic until canonical record
marshalling is implemented. Records use canonical field layouts at a component
boundary; JSON payloads should be declared as WIT `string` and parsed explicitly.
The SDK can describe composite WIT types beyond the compiler's current ABI support.

`fetch` supports GET and POST, string request bodies, and headers supplied as a
plain object or an array of name/value pairs. Other request options fail with a
diagnostic before sending the request. HTTP error statuses remain readable
responses with `status` and `ok` properties.
