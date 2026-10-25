# Semantics

Perry-WIT accepts a statically typed subset of TypeScript.
Names, object layouts and host capabilities are resolved at compile time, and a construct outside the subset is a compile error with a source location.

## Objects

- Interfaces, type aliases and object literals compile to records with fixed field offsets. Types with the same fields share a layout; classes and recursive types get their own.
- Records keep their declared fields, and prototype mutation is rejected. `Record<string, V>` dictionaries accept new keys, and `delete` removes dictionary keys and optional properties.
- An empty object, including `const object: any = {}`, becomes a dictionary when every write stores the same value type. Writes through local aliases, branches and loops count. An object that escapes, or whose writes store different types, needs a type annotation.
- Unions of literals, records and WIT variants narrow through `typeof`, equality and discriminant properties.

## Arrays

- Arrays (`T[]`) are dense. Elements share one type or a finite union of numbers, strings, booleans and one plain record shape.
- Reading an index outside the array, or a fractional index, yields `undefined` when the element type admits it.
- Writing at `length` appends. Writing at any other index outside `0..length` throws `RangeError`.
- Fresh empty arrays (`[]`, including `any[]`) infer their element type from `push` calls and numeric-indexed writes across branches, loops, non-reassigned local aliases and helper returns, widening string literals to `string` and preserving nested record and array layouts. Inferred arrays support `.length`, index reads, `Object.keys`, `Array.isArray` and `JSON.stringify` without a replacer; other uses, unresolved writes or no writes require an explicit element type.

## Strings

- Strings are UTF-8. Length and indexing count Unicode scalar values and agree with Node.js for text in the Basic Multilingual Plane. Only `charCodeAt` indexes UTF-16 code units, so past a character outside that plane its positions differ from `length` and `indexOf`: `'😀x'.length` is 2, yet `'😀x'.charCodeAt(2)` reads the `x`.
- `String.fromCharCode` and lone surrogate escapes (`\uD800` to `\uDFFF`) are compile errors. Use `String.fromCodePoint`.
- Text arriving through the Canonical ABI, `JSON.parse` or `TextDecoder` is validated as UTF-8.

## Regular Expressions

- Regular expressions require compile-time constant patterns and flags (regex literals or `RegExp` calls with string literal arguments) and compile to search bytecode.
- `test`, `exec`, `match`, `search`, `split`, `replace` and `replaceAll` support named capture groups (`.groups` on `exec` and non-global `match`), `$<name>` replacement templates and synchronous replacement callbacks.

## JSON

- `JSON.parse` decodes into the expected static target type (retaining extra object properties for serialization) or into a dynamic JSON value that is validated when assigned to a typed binding. Reviver callbacks are compile errors.
- `JSON.stringify` supports synchronous replacer callbacks and property-allowlist arrays. The third indentation argument is a compile error.

## Dates

- `Date` operates in the UTC time zone. `Date.parse` and `new Date(string)` accept ISO 8601 year, year-month, date and date-time strings (offset-free strings use UTC). Malformed ISO strings return `NaN`, whereas non-ISO legacy date formats throw `RangeError`.
- `Temporal.Instant`, `Temporal.PlainDate` and `Temporal.PlainDateTime` use the ISO 8601 calendar (`calendarId` is `"iso8601"`, and non-ISO calendar annotations are rejected). `PlainDate.from` and `PlainDateTime.from` accept ISO strings, existing instances or field objects (`year`, `month`, `day` and optional `hour` through `nanosecond`) with `{ overflow: "constrain" | "reject" }`. `add` and `subtract` accept `{ years?, months?, weeks?, days? }` with optional `overflow`, and `until` and `since` accept `largestUnit` (`"day"`, `"week"`, `"month"`, `"year"`, `"auto"`).

## Exceptions

- `Error`, `TypeError`, `RangeError`, `ReferenceError` and `SyntaxError` keep `name`, `message` and `cause` through `try`, `catch` and `finally`.
- A failed implicit check produces a fresh error object when caught. Successful checks bypass the exception runtime; explicitly thrown values retain their identity.
- An export declared with a WIT `result<T, E>` turns an uncaught exception into `err`. Without a `result`, an uncaught exception traps, and a trap does not run pending `finally` blocks.

## Async

- An `async` function runs eagerly until its first suspension. Awaiting an already settled promise continues inline.
- There is no microtask queue in the guest. Waiting tasks resume from WASI 0.3 callbacks, so the order of continuations for different host events follows the host, not Node.js. Observers of the same promise resume in registration order.
- `Promise.race` leaves the losing tasks running. They must settle before the export returns, or the call traps.
- `ReadableStream` supports default readers (`getReader()`), `for await` loops, BYOB readers (`getReader({ mode: "byob" })` with `read(view, { min })` into a caller-provided `Uint8Array`) and explicit cancellation. `for await` over a byte stream releases the reader lock and cancels the stream when the loop exits early. Releasing a reader lock rejects pending reads but keeps buffered chunks for the next reader.

## Numbers

WIT provides fixed-width integers (`u8`..`u64`, `s8`..`s64`) alongside `f32` and `f64`, while TypeScript provides `number` (IEEE 754 `f64`) and `bigint`.

| WIT type | `.d.ts` type | Guest representation |
| :--- | :--- | :--- |
| `u8`, `u16`, `u32`, `s8`, `s16`, `s32` | `number` | Refined `number` (`i32`, `i64` or `f64`) |
| `u64`, `s64` | `bigint` | `i64` at boundaries, arbitrary-width `bigint` in guest |
| `f32`, `f64` | `number` | `f64` |

Keeping `u8`..`s32` as `number` in generated `.d.ts` files lets guest code use standard TypeScript arithmetic while giving the compiler range information to emit integer instructions.

### Refinement Analysis

Each numeric SSA value carries an optional integer type (width, signedness) and a value interval `[min, max]`.
`ProgramFacts` propagates ranges forward through arithmetic, branches and calls (including recursion), and propagates required register widths and observation requirements backward through the same worklist to a fixed point [[1]](#ref-1).

- **Register selection**: Refined integers whose interval fits in 32 bits lower to `i32`. Wider refined integers lower to `i64`. Unrefined values stay in `f64`.
- **Bitwise results**: `&`, `|`, `^`, `~`, `<<`, `>>` and `Math.imul` produce `s32` values and `>>>` produces `u32`, so they lower to `i32` instructions even when their operands are unrefined `f64` values, which convert with `ToInt32`. When every use needs only the low bits, refined arithmetic feeding them, such as `(a * b) | 0` on `u32` values, computes in `i32`. This deliberately preserves integer wrapping before conversion: at `a = b = 4294967295`, the result is `1`, whereas JavaScript rounds the product before converting and returns `0`. A use that observes the full value, such as arithmetic or a comparison, requires a wider register when the result does not fit; a bitwise consumer then truncates at its own use.
- **Register-width intermediates**: Required widths stay at register size (32 or 64 bits) during computation. Narrowing to 8 or 16 bits happens only at the WIT boundary.
- **Literals**: An integer literal adopts a required width only when its value fits. Larger literals stay in `f64` until converted at the boundary.
- **Loops**: At a join point, an interval may grow once before widening to the bounds of its register type, or to unbounded endpoints for unrefined numbers. A specialized loop counter wraps at 32 or 64 bits on overflow.
- **Guarded ranges**: Dominating comparisons of integral operands refine index ranges through the numeric solver’s shared arithmetic and integer bounds. Derived arithmetic and cast ranges must fit the result type. Addition and subtraction preserve one-sided bounds; arithmetic that may produce `NaN` stays unknown.
- **Bounds checks**: A proof of `0 <= i < length` permits unchecked array reads and stores, and `Uint8Array` reads. Array proofs account for mutations through aliases. Byte-view proofs use length comparisons or constructor, copy and subarray extents, and require the view to stay attached on every path to the access. Calls, awaits, foreign operations and BYOB reads invalidate these byte-view proofs.

#### Type Join

Combining two operands takes the least upper bound of their refinements:

| Operands | Join | Reason |
| :--- | :--- | :--- |
| `u8`, `u16` | `u16` | Wider type of the same signedness. |
| `u8`, `s8` | `s16` | Holds both `0..=255` and `-128..=127`. |
| `u16`, `s16` | `s32` | Next signed width up. |
| `u32`, `s32` | `s64` | Next signed width up. |
| `u8`, `s32` | `s32` | Signed operand is already wider. |
| integer, unrefined `number` | `f64` | Unrefined `number` may be fractional. |

### Generated Code

```wit check=numerics
package example:numerics@0.1.0;

world numerics {
  export combine-delta: func(base: u8, delta: s8) -> s16;
  export scale-bytes: func(count: u32, stride: u32) -> u32;
}
```

```ts check=numerics
export function combineDelta(base: number, delta: number): number {
  return base + delta;
}

export function scaleBytes(count: number, stride: number): number {
  return Math.trunc((count * stride) / 1024);
}
```

Compiling this pair with `--core-only` emits:

{{#include numerics-wat.md}}

- **`combine-delta`**: The export adapter `$wit_combine_delta` masks `base` (`i32.and`) and sign-extends `delta` (`i32.extend8_s`). The sum lies in `-128..=382`, so the body adds directly in `i32`.
- **`scale-bytes`**: The product of two `u32` values is below 2<sup>64</sup>, so `i64.mul` computes it exactly. Converting that exact product to `f64` rounds once, matching JavaScript floating-point multiplication. `/` is floating-point division, so the division stays in `f64`.

### Boundary Conversion

- **Incoming `u8`..`s16`**: The export adapter masks `u8` and `u16` arguments and sign-extends `s8` and `s16` arguments.
- **Outgoing `u8`..`s32`**: Values convert with JavaScript typed-array semantics (`ToUint8` through `ToInt32`), truncating toward zero, mapping `NaN` and `±Infinity` to `0`, and wrapping modulo 2<sup>N</sup>. Integer and `f64` code paths produce identical results.
- **`u64`, `s64` (`bigint`)**: Out-of-range `bigint` returns trap at the boundary. Inside guest code, `+`, `-`, unary `-`, comparisons and `toString()` support arbitrary width. `*`, `/`, `%` and `BigInt(number)` require 64-bit operands.

```wit check=boundaries
package example:boundaries@0.1.0;

world boundaries {
  export scale8: func(a: u8, b: u8) -> u8;
  export to-u8: func(x: f64) -> u8;
  export to-s32: func(x: f64) -> s32;
  export dec-u64: func(x: u64) -> u64;
}
```

```ts check=boundaries
export function scale8(a: number, b: number): number { return a * b; }
export function toU8(x: number): number { return x; }
export function toS32(x: number): number { return x; }
export function decU64(x: bigint): bigint { return x - 1n; }
```

<!-- invoke: boundaries -->
| Call | Result | JavaScript |
| :--- | :--- | :--- |
| `scale8(200, 2)` | `144` | `new Uint8Array([400])[0]` |
| `to-u8(-1)` | `255` | `new Uint8Array([-1])[0]` |
| `to-s32(3000000000)` | `-1294967296` | `3000000000 \| 0` |
| `dec-u64(1)` | `0` | `1n - 1n` |
| `dec-u64(0)` | trap | `-1n` is outside `u64` |

## References

1. <a id="ref-1"></a>M. Stephenson, J. Babb, S. Amarasinghe. [*Bitwidth Analysis with Application to Silicon Compilation*](https://doi.org/10.1145/349299.349317). ACM PLDI, 2000.
