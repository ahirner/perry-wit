# Numbers

TypeScript has one `number` type (IEEE 754 binary64) plus `bigint`.
WIT has `u8` through `u64`, `s8` through `s64`, `f32` and `f64`.
Perry-WIT reads a WIT integer type in a signature as a refinement of `number`: a `u8` parameter is a `number` that is an integer in `0..=255`.
Generated `.d.ts` files still declare `number`, so sources stay valid TypeScript, while the compiler keeps the refinement and uses it to choose machine types.

## Inferred Refinements

Every numeric SSA value carries a refinement made of an optional integer type (width and signedness) and an interval `[min, max]`.
`ProgramFacts` propagates refinements forward through arithmetic, branches and calls (recursive calls included) and required widths backward, until fixed point.
An interval at a join point may grow exactly once; after that it widens to the bounds of its integer type, so loops converge.
After the analysis, a value with an integer refinement lowers to `i32` when it fits 32 bits and to `i64` otherwise; a value without one stays `f64`.

Combining two integer types takes their least upper bound:

| Operands | Join | Reason |
| :--- | :--- | :--- |
| `u8`, `u16` | `u16` | Same signedness: the wider type. |
| `u8`, `s8` | `s16` | Must hold both `0..=255` and `-128..=127`. |
| `u16`, `s16` | `s32` | Same, one width up. |
| `u32`, `s32` | `s64` | Same, one width up. |
| `u8`, `s32` | `s32` | The signed type is already wider. |
| any integer, unrefined `number` | `f64` | An unrefined `number` may be fractional. |

The interval then decides whether a result fits a 32-bit register and, for an array index, whether the bounds check can be dropped.

## Generated Code

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

- **`combine-delta`**: The export adapter `$wit_combine_delta` masks `base` (`i32.and`) and sign-extends `delta` (`i32.extend8_s`). The sum lies in `-128..=382`, so the body adds in `i32` with no float conversion.
- **`scale-bytes`**: The product of two `u32` values is below 2<sup>64</sup>, so `i64.mul` computes it exactly. Converting that exact product to `f64` rounds once, which gives the same value as JavaScript's floating-point multiplication. `/` is not integer division, so the division stays in `f64`.

## Boundaries

The export adapter masks incoming `u8` and `u16` arguments and sign-extends `s8` and `s16` arguments.
Outgoing values follow two rules:

- A result computed on the integer path wraps modulo 2<sup>N</sup>.
- A result computed in `f64` truncates toward zero and saturates to the 32-bit range of its signedness (`NaN` becomes `0`). An 8- or 16-bit result then keeps its low bits.

ECMAScript's `ToUint32` and `ToInt32` wrap modulo 2<sup>32</sup> instead, so `f64` results outside the 32-bit range differ from what JavaScript would produce.
`u64` and `s64` appear as `bigint` in `.d.ts` files; their bits pass through unchanged, and a `bigint` result outside the declared type traps, even when the export returns a `result`.
Within guest code, `bigint` supports `+`, `-`, unary `-`, comparison and `toString()` at any width; `*`, `/`, `%` and `BigInt(number)` work on values that fit in 64 bits.

```wit check=boundaries
package example:boundaries@0.1.0;

world boundaries {
  export scale8: func(a: u8, b: u8) -> u8;
  export add32: func(a: u32, b: u32) -> u32;
  export to-u8: func(x: f64) -> u8;
  export to-s8: func(x: f64) -> s8;
  export to-u32: func(x: f64) -> u32;
  export to-s32: func(x: f64) -> s32;
  export to-u64: func(x: s64) -> u64;
  export dec-u64: func(x: u64) -> u64;
}
```

```ts check=boundaries
export function scale8(a: number, b: number): number { return a * b; }
export function add32(a: number, b: number): number { return a + b; }
export function toU8(x: number): number { return x; }
export function toS8(x: number): number { return x; }
export function toU32(x: number): number { return x; }
export function toS32(x: number): number { return x; }
export function toU64(x: bigint): bigint { return x; }
export function decU64(x: bigint): bigint { return x - 1n; }
```

<!-- invoke: boundaries -->
| Call | Result | Rule |
| :--- | :--- | :--- |
| `scale8(200, 2)` | `144` | 400 wraps modulo 2<sup>8</sup>. |
| `add32(4294967295, 1)` | `0` | Wraps modulo 2<sup>32</sup>. |
| `to-u8(300.7)` | `44` | Truncates to 300, then keeps the low 8 bits. |
| `to-u8(-1)` | `0` | Saturates at 0 before narrowing. |
| `to-s8(200)` | `-56` | Keeps the low 8 bits as a signed value. |
| `to-u32(4294967301)` | `4294967295` | Saturates at 2<sup>32</sup> - 1. |
| `to-s32(3000000000)` | `2147483647` | Saturates at 2<sup>31</sup> - 1. |
| `to-u64(-1)` | `18446744073709551615` | Bits pass through unchanged. |
| `dec-u64(0)` | trap | -1 is outside `u64`. |
