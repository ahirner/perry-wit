# Semantics

Perry-WIT compiles a deterministic, statically typed subset of TypeScript designed for ahead-of-time WebAssembly component execution. Lexical bindings, record layouts, and capability requirements resolve completely at compile time.

## Value Representations

### Records

Interfaces, type aliases, and object literals compile to closed structural layouts on the guest heap:

- **Structural Equivalence**: Distinct named interfaces or inline object type literals with identical field sets share a single physical layout.
- **Closed Shapes**: Property accesses resolve to fixed field offsets at compile time. Dynamic property insertion, `delete`, and prototype mutation are rejected during source validation.
- **Recursive Definitions**: Self-referential and mutually recursive interface definitions lower through boxed heap pointers traced by root frames.
- **Discriminated Unions**: Finite unions of primitive literals, records, and WIT variant cases lower to tagged representations checked via `typeof`, equality guards, or discriminant properties.

### Arrays

Typed arrays (`T[]`) are dense, homogeneous sequences backed by contiguous guest memory:

- **Read Semantics**: Indexing with an out-of-bounds integer or non-integral float evaluates to `undefined` without trapping, matching ECMAScript array access semantics where the return type permits `undefined`.
- **Append Semantics**: Assigning to `array[array.length]` appends a new element, growing the backing buffer with amortized doubling capacity.
- **Bounds Enforcement**: Assigning to any index outside `0 <= i <= array.length` throws a catchable `RangeError`. When `ProgramFacts` proves an index is within bounds, the code generator elides the runtime check.

### Strings

Strings are immutable UTF-8 byte sequences paired with a cached Unicode scalar count:

- **Scalar Indexing**: Indexing, `.length`, `.slice()`, and `.substring()` operate on Unicode scalar values. Basic Multilingual Plane (BMP) text matches Node.js behavior directly; astral characters count as single scalar values.
- **Boundary Validation**: Lone UTF-16 surrogates and code-unit intrinsics (`charCodeAt`, `\uD800`–`\uDFFF`) are rejected at compile time. Inbound text crossing Canonical ABI boundaries, `JSON.parse`, or `TextDecoder` undergoes strict UTF-8 validation.
- **Amortized Concatenation**: Repeated string appends reuse spare capacity in the underlying allocation for linear-time growth.

## Control Flow

### Structured Execution

Perry-WIT lowers standard structured control flow constructs directly to SSA basic blocks and WebAssembly structured instructions:

- Conditional branches (`if` / `else`, `switch`, ternary `?:`) and short-circuit boolean operators (`&&`, `||`, `??`) preserve left-to-right operand evaluation order.
- Loops (`while`, `for`, `for...of`, `do...while`) support nested `break` and `continue` targets lowered via Ramsey's dominator-tree algorithm.
- Module-scope top-level statements execute once inside a guarded initializer before the first exported function invocation, persisting module bindings across subsequent calls on the same instance.

### Exception Propagation

Standard error classes (`Error`, `TypeError`, `RangeError`, `ReferenceError`, `SyntaxError`) propagate through structured unwinding paths:

- **Lexical Unwinding**: `try`, `catch`, and `finally` blocks establish shared cleanup blocks in MIR. Nested `finally` blocks execute in deterministic inner-to-outer order across normal exits, `return` statements, and thrown exceptions.
- **Error Chaining**: Error instances preserve `message`, `name`, and nested `cause` values across rethrows.
- **Boundary Mapping**: When an exported function maps to a WIT `result<T, E>` signature, uncaught exceptions lower into the `err` variant or a structured `{ ok: false, error }` payload. Uncaught exceptions escaping an export without a result channel trigger a deterministic WebAssembly trap.

## Asynchronous Execution

### Promise Settlement

Asynchronous functions (`async function` / `Promise<T>`) execute eagerly until they reach a host suspension point:

- **Inline Continuation**: Awaiting an already-settled `Promise` continues synchronously on the current guest stack without microtask queue overhead.
- **Repeated Observation**: Settled promises retain their outcome in the guest task record so multiple `await` expressions borrow the same settled value without cloning its heap graph.
- **Combinators**: `Promise.all`, `Promise.race`, and `Promise.allSettled` register observer links directly on operand task records. In `Promise.race`, non-winning tasks continue executing in the background; the export adapter verifies that all spawned tasks settle or acknowledge cancellation before the component invocation returns.

### Byte Streams

`ReadableStream<Uint8Array>` and `WritableStream<Uint8Array>` bridge Web Platform stream APIs directly to WASI 0.3 native streams:

- **Default Readers**: `getReader()` locks the stream and pulls chunks incrementally across the host boundary. Releasing a reader lock rejects pending read promises while preserving unread buffered chunks for subsequent readers.
- **BYOB Readers**: `getReader({ mode: "byob" })` reads directly into caller-supplied `Uint8Array` views, honoring `min` byte length thresholds without intermediate copies.
- **Async Iteration**: `for await (const chunk of stream)` acquires a locked reader and guarantees deterministic cancellation and lock release on early `break`, `return`, or thrown exceptions.

## Standard Library Inventory

The table below catalogs every built-in global and module surface supported by Perry-WIT, its underlying runtime or WASI 0.3 backing, and its static or runtime boundary constraints.

| Surface | Supported Capabilities | WASI 0.3 Backing | Boundary Constraints |
| :--- | :--- | :--- | :--- |
| **`Math`** | `abs`, `min`, `max`, `floor`, `ceil`, `round`, `trunc`, `sqrt`, `pow`, `log`, `exp`, trigonometric functions, constants (`PI`, `E`) | Core Wasm `f64` instructions + pure guest helpers | Evaluates in IEEE 754 `f64`; `NaN` and `-0` preserved. |
| **`JSON`** | Schema-directed `JSON.parse<T>()` and `JSON.stringify()`, replacer callbacks, property-list arrays, indentation | Specialized MIR decoders + embedded Rust JSON helper | Requires static target type on `JSON.parse`; rejects circular structures with `TypeError`. |
| **`Uint8Array` / `ArrayBuffer`** | Construction, `.subarray()`, `.slice()`, `.set()`, `.fill()`, `.byteLength`, `.byteOffset`, indexed element access | Contiguous guest heap byte buffers | Canonical ABI `list<u8>` maps zero-copy to `Uint8Array` views. |
| **`TextEncoder` / `TextDecoder`** | `.encode()`, `.encodeInto()`, `.decode()` with `fatal` and `ignoreBOM` options | Embedded UTF-8 validation helper | Supports UTF-8 encoding exclusively; non-UTF-8 labels rejected at compile time. |
| **`RegExp` / `String` matchers** | Literal regular expressions, `.test()`, `.exec()`, `.match()`, `.replace()`, `.replaceAll()`, `.split()`, named capture `.groups`, replacement callbacks | Compile-time NFA/DFA synthesis + guest matcher helper | Requires static regex literals; dynamic `new RegExp(variable)` and backreferences rejected at compile time. |
| **`Date`** | `Date.now()`, `new Date(ms \| isoString)`, `Date.parse()`, `Date.UTC()`, `.toISOString()`, UTC field getters (`getUTCFullYear`, etc.) | `wasi:clocks/wall-clock@0.3.0` + pure ISO calendar helper | Operates strictly in UTC; malformed ISO strings return `NaN`, non-ISO legacy strings throw `RangeError`. |
| **`Temporal`** | `Temporal.Instant.from`, `Temporal.PlainDate.from`, `Temporal.PlainDateTime.from`, `.add({ days })`, `.epochMilliseconds`, ISO formatting | Pure guest calendar arithmetic helper | Supports ISO 8601 calendar with `overflow: "constrain" \| "reject"`; custom calendars and time zones rejected at compile time. |
| **`crypto`** | `crypto.getRandomValues(typedArray)`, `crypto.randomUUID()` | `wasi:random/random@0.3.0` | Quota exceeding `65536` bytes per `getRandomValues` call throws `TypeError`. |
| **`URL` / `URLSearchParams`** | `new URL(input, base?)`, `href`, `origin`, `protocol`, `host`, `pathname`, `search`, `hash`, `searchParams` manipulation | Embedded WHATWG URL parser helper | Relative URLs require an explicit `base` argument; invalid URLs throw `TypeError`. |
| **`Headers` / `Request` / `Response`** | Case-insensitive header maps, streaming bodies (`.text()`, `.json()`, `.bytes()`, `.arrayBuffer()`, `.body`), status codes | `wasi:http/types@0.3.0` + stream bridges | Forbidden header modifications or locked body reuse throw `TypeError`. |
| **`fetch`** | Outbound asynchronous HTTP requests with streaming request and response bodies | `wasi:http/client@0.3.0` | Requires a WIT world importing `wasi:http/client@0.3.0`. |
| **`console`** | `console.log()`, `console.error()`, `console.warn()`, `console.info()` | `wasi:cli/stdout@0.3.0`, `wasi:cli/stderr@0.3.0` | Drains line-by-line through asynchronous WASI 0.3 output streams. |
| **`process`** | `process.env`, `process.argv`, `process.exit(code)`, `process.exitCode` | `wasi:cli/environment@0.3.0`, `wasi:cli/exit@0.3.0` | Available in CLI and server worlds importing WASI CLI interfaces. |
| **`node:timers/promises`** | `setTimeout(ms, value?)` | `wasi:clocks/monotonic-clock@0.3.0` | Suspends the current guest task on the WASI 0.3 monotonic timer handle. |
| **`node:fs/promises`** | `readFile`, `writeFile`, `appendFile`, `mkdir`, `readdir`, `stat`, `rm`, `rename`, `open` (`FileHandle`) | `wasi:filesystem/types@0.3.0`, `wasi:filesystem/preopens@0.3.0` | Paths resolve relative to preopened host directories; escaping paths reject with `EACCES` / `ENOENT`. |
| **`node:path`** | `join`, `resolve`, `dirname`, `basename`, `extname`, `normalize`, `isAbsolute`, `sep`, `posix` | Pure guest path normalization helper | Uses POSIX `/` path semantics across all host platforms. |
| **`node:buffer` / `atob` / `btoa`** | `Buffer.from()`, `Buffer.alloc()`, `Buffer.concat()`, UTF-8, `hex`, `base64`, `base64url` codecs, global `atob` / `btoa` | Guest byte buffer views + codec helpers | `Buffer` instances interoperate directly with `Uint8Array`. |
| **`node:util` / `node:assert`** | `TextEncoder`, `TextDecoder`, `format`, `assert.strictEqual`, `assert.deepStrictEqual`, `assert.throws`, `assert.rejects` | Pure guest formatting and structural comparison helpers | Assertion failures throw catchable `Error` instances with diff diagnostics. |
