# Semantics

Perry-WIT accepts a statically typed subset of TypeScript.
Names, object layouts and host capabilities are resolved at compile time, and a construct outside the subset is a compile error with a source location.

## Objects

- Non-recursive interfaces, type aliases and object literals compile to closed layouts with fixed field offsets, and types with the same fields share one structural layout. Nominal classes and recursive types allocate distinct record layouts.
- Adding properties at run time and mutating prototypes are rejected. `delete` works on optional properties and on `Record<K, V>` maps.
- Unions of literals, records and WIT variants narrow through `typeof`, equality and discriminant properties.

## Arrays

- Arrays (`T[]`) are dense and homogeneous.
- Reading an index outside the array, or a fractional index, yields `undefined` when the element type admits it.
- Writing at `length` appends. Writing at any other index outside `0..length` throws `RangeError`.
- Fresh empty arrays (`[]`, including `any[]`) infer a static element type from uniform `push` calls and numeric-indexed writes across branches, loops and non-reassigned local aliases, widening string literals to `string` and preserving nested record and array layouts. Inferred arrays support `.length`, primitive index reads and `JSON.stringify` without a replacer; other uses, conflicting writes or no writes require an explicit element type.

## Strings

- Strings are UTF-8 and index by Unicode scalar value, not UTF-16 code unit. Indices agree with Node.js for text in the Basic Multilingual Plane. Characters outside it count as one scalar value.
- `charCodeAt`, `String.fromCharCode` and lone surrogate escapes (`\uD800` to `\uDFFF`) are compile errors. Use `codePointAt` and `String.fromCodePoint`.
- Text arriving through the Canonical ABI, `JSON.parse` or `TextDecoder` is validated as UTF-8.

## Regular Expressions

- Regular expressions require compile-time constant patterns and flags (regex literals or `RegExp` calls with string literal arguments) and compile to search bytecode.
- `test`, `exec`, `match`, `search`, `split`, `replace` and `replaceAll` support named capture groups (`.groups` on `exec` and non-global `match`), `$<name>` replacement templates and synchronous replacement callbacks.

## JSON

- `JSON.parse` decodes into the expected static target type (ignoring extra object properties not in the target shape) or into a dynamic JSON value that is validated when assigned to a typed binding. Reviver callbacks are compile errors.
- `JSON.stringify` supports synchronous replacer callbacks and property-allowlist arrays. The third indentation argument is a compile error.

## Dates

- `Date` operates in the UTC time zone. `Date.parse` and `new Date(string)` accept ISO 8601 year, year-month, date and date-time strings (offset-free strings use UTC). Malformed ISO strings return `NaN`, whereas non-ISO legacy date formats throw `RangeError`.
- `Temporal.Instant`, `Temporal.PlainDate` and `Temporal.PlainDateTime` use the ISO 8601 calendar (`calendarId` is `"iso8601"`, and non-ISO calendar annotations are rejected). `PlainDate.from` and `PlainDateTime.from` accept ISO strings, existing instances or field objects (`year`, `month`, `day` and optional `hour` through `nanosecond`) with `{ overflow: "constrain" | "reject" }`. `add` and `subtract` accept `{ years?, months?, weeks?, days? }` with optional `overflow`, and `until` and `since` accept `largestUnit` (`"day"`, `"week"`, `"month"`, `"year"`, `"auto"`).

## Exceptions

- `Error`, `TypeError`, `RangeError`, `ReferenceError` and `SyntaxError` keep `name`, `message` and `cause` through `try`, `catch` and `finally`.
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
`ProgramFacts` propagates ranges forward through arithmetic, branches and calls (including recursion) and propagates required register widths backward to a fixed point [[1]](#ref-1).

- **Register selection**: Refined integers whose interval fits in 32 bits lower to `i32`. Wider refined integers lower to `i64`. Unrefined values stay in `f64`.
- **Bitwise results**: `&`, `|`, `^`, `~`, `<<`, `>>` and `Math.imul` produce `s32` values and `>>>` produces `u32`, so they lower to `i32` instructions even when their operands are unrefined `f64` values, which convert with `ToInt32`. Only the low 32 bits of each operand reach the result, so refined arithmetic feeding them, such as `(a * b) | 0` on `u32` values, computes in `i32`.
- **Register-width intermediates**: Required widths stay at register size (32 or 64 bits) during computation. Narrowing to 8 or 16 bits happens only at the WIT boundary.
- **Literals**: An integer literal adopts a required width only when its value fits. Larger literals stay in `f64` until converted at the boundary.
- **Loops**: At a join point, an interval may grow once before widening to the bounds of its register type. A specialized loop counter wraps at 32 or 64 bits on overflow.
- **Bounds checks**: When an index interval proves `0 <= i < length`, the array read elides its bounds check.

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
