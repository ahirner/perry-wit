# Numbers

WIT provides fixed-width integers (`u8`..`u64`, `s8`..`s64`) alongside `f32` and `f64`, while TypeScript provides `number` (IEEE 754 `f64`) and `bigint`.

| WIT type | `.d.ts` type | Guest representation |
| :--- | :--- | :--- |
| `u8`, `u16`, `u32`, `s8`, `s16`, `s32` | `number` | Refined `number` (`i32`, `i64` or `f64`) |
| `u64`, `s64` | `bigint` | `i64` at boundaries, arbitrary-width `bigint` in guest |
| `f32`, `f64` | `number` | `f64` |

Keeping `u8`..`s32` as `number` in generated `.d.ts` files lets guest code use standard TypeScript arithmetic while giving the compiler range information to emit integer instructions.

## Refinement Analysis

Each numeric SSA value carries an optional integer type (width, signedness) and a value interval `[min, max]`.
`ProgramFacts` propagates ranges forward through arithmetic, branches and calls (including recursion) and propagates required register widths backward to a fixed point [[1]](#ref-1).

- **Register selection**: Refined integers whose interval fits in 32 bits lower to `i32`. Wider refined integers lower to `i64`. Unrefined values stay in `f64`.
- **Register-width intermediates**: Required widths stay at register size (32 or 64 bits) during computation. Narrowing to 8 or 16 bits happens only at the WIT boundary.
- **Literals**: An integer literal adopts a required width only when its value fits. Larger literals stay in `f64` until converted at the boundary.
- **Loops**: At a join point, an interval may grow once before widening to the bounds of its register type. A specialized loop counter wraps at 32 or 64 bits on overflow.
- **Bounds checks**: When an index interval proves `0 <= i < length`, the array read elides its bounds check.

### Type Join

Combining two operands takes the least upper bound of their refinements:

| Operands | Join | Reason |
| :--- | :--- | :--- |
| `u8`, `u16` | `u16` | Wider type of the same signedness. |
| `u8`, `s8` | `s16` | Holds both `0..=255` and `-128..=127`. |
| `u16`, `s16` | `s32` | Next signed width up. |
| `u32`, `s32` | `s64` | Next signed width up. |
| `u8`, `s32` | `s32` | Signed operand is already wider. |
| integer, unrefined `number` | `f64` | Unrefined `number` may be fractional. |

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
- **`scale-bytes`**: The product of two `u32` values is below 2<sup>64</sup>, so `i64.mul` computes it exactly. Converting that exact product to `f64` rounds once, matching JavaScript floating-point multiplication. `/` is floating-point division, so the division stays in `f64`.

## Boundary Conversion

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
