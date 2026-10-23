# Numerical Tower

JavaScript defines a single `number` type backed by IEEE 754 double-precision floats (`f64`), alongside arbitrary-precision `bigint`. By contrast, the WebAssembly Component Model and Canonical ABI expose eight distinct fixed-width integer types (`u8`, `u16`, `u32`, `u64`, `s8`, `s16`, `s32`, `s64`) and two floating-point widths (`f32`, `f64`).

Perry-WIT bridges these type systems through a stratified **Numerical Tower** that preserves ECMAScript semantics for unconstrained expressions while specializing WIT-constrained paths to native WebAssembly integer instructions.

## Representation Stratification

Every numeric SSA value in Perry-WIT carries a static representation lattice type determined by WIT boundary signatures, explicit type annotations, and interprocedural `ProgramFacts` inference:

| Source Type / Contract | MIR Representation | Core Wasm Storage | Runtime Semantics |
| :--- | :--- | :--- | :--- |
| Unconstrained `number` | `Float64` | `f64` | Full IEEE 754 semantics: fractional division, `-0`, `Infinity`, `NaN` propagation. |
| WIT `f32` / `f64` | `Float32` / `Float64` | `f32` / `f64` | Canonical ABI float marshalling (`f32.demote_f64` / `f64.promote_f32` at boundaries). |
| WIT `u8`, `u16`, `u32` | `UInt8`, `UInt16`, `UInt32` | `i32` | Unsigned modular or range-checked integer operations in 32-bit registers. |
| WIT `s8`, `s16`, `s32` | `SInt8`, `SInt16`, `SInt32` | `i32` | Two's-complement signed integer operations in 32-bit registers. |
| Intermediate Integer | `Int64` (register-promoted) | `i64` | Overflow-free intermediate arithmetic for narrow integer expressions (`u8`–`s32`). |
| WIT `u64`, `s64` | `UInt64`, `SInt64` | `i64` | Mapped to TypeScript `bigint` at the language level; marshalled in native `i64` registers. |
| Unconstrained `bigint` | `BigInt` | `i64` / heap limb buffer | Exact arbitrary-width `+`, `-`, unary `-`, comparison, and decimal string conversion. |

## Subsumption Join Lattice

When a binary expression combines operands of differing numeric widths or signedness, Perry-WIT computes the **least upper bound (subsumption join)** capable of representing every value of both inputs without precision loss:

- **Same-Signedness Widening**: Operands of identical signedness promote to the wider bitwidth (`u8 ⊔ u16 = u16`, `s16 ⊔ s32 = s32`).
- **Cross-Signedness Widening**: Combining an unsigned integer of width $N$ with a signed integer of width $\le N$ promotes to the next enclosing signed width (`u8 ⊔ s8 = s16`, `u16 ⊔ s16 = s32`, `u32 ⊔ s32 = s64`), because `s8` (`-128..127`) cannot hold `128..255` and `u8` (`0..255`) cannot hold `-128..-1`.
- **Float Dominance**: Combining any integer width (`u8`–`s32`) with an unconstrained `number` (`f64`) promotes to `f64`, because IEEE 754 `f64` has 53 bits of significand and exactly subsumes all 32-bit integers.

<figure class="pw-diagram">
<svg viewBox="0 0 760 360" xmlns="http://www.w3.org/2000/svg" role="img" aria-label="Perry-WIT Numerical Tower Subsumption Join Lattice">
  <defs>
    <marker id="arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 1 L 8 5 L 0 9 z" fill="currentColor" opacity="0.65" />
    </marker>
    <marker id="arrow-accent" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 1 L 8 5 L 0 9 z" fill="#3b82f6" />
    </marker>
  </defs>
  <text x="20" y="52" fill="currentColor" opacity="0.6" font-size="11" font-weight="600">8-BIT</text>
  <text x="20" y="132" fill="currentColor" opacity="0.6" font-size="11" font-weight="600">16-BIT</text>
  <text x="20" y="212" fill="currentColor" opacity="0.6" font-size="11" font-weight="600">32-BIT</text>
  <text x="20" y="292" fill="currentColor" opacity="0.6" font-size="11" font-weight="600">64-BIT / TOP</text>
  <line x1="160" y1="66" x2="160" y2="104" stroke="currentColor" stroke-opacity="0.45" stroke-width="1.5" marker-end="url(#arrow)" />
  <line x1="160" y1="146" x2="160" y2="184" stroke="currentColor" stroke-opacity="0.45" stroke-width="1.5" marker-end="url(#arrow)" />
  <line x1="160" y1="226" x2="160" y2="264" stroke="currentColor" stroke-opacity="0.45" stroke-width="1.5" marker-end="url(#arrow)" />
  <line x1="360" y1="66" x2="360" y2="104" stroke="currentColor" stroke-opacity="0.45" stroke-width="1.5" marker-end="url(#arrow)" />
  <line x1="360" y1="146" x2="360" y2="184" stroke="currentColor" stroke-opacity="0.45" stroke-width="1.5" marker-end="url(#arrow)" />
  <line x1="360" y1="226" x2="360" y2="264" stroke="currentColor" stroke-opacity="0.45" stroke-width="1.5" marker-end="url(#arrow)" />
  <line x1="205" y1="58" x2="312" y2="114" stroke="#3b82f6" stroke-width="2" marker-end="url(#arrow-accent)" />
  <line x1="205" y1="138" x2="312" y2="194" stroke="#3b82f6" stroke-width="2" marker-end="url(#arrow-accent)" />
  <line x1="205" y1="218" x2="312" y2="274" stroke="#3b82f6" stroke-width="2" marker-end="url(#arrow-accent)" />
  <path d="M 210 206 C 390 206, 470 250, 540 272" fill="none" stroke="currentColor" stroke-opacity="0.4" stroke-dasharray="4 3" stroke-width="1.5" marker-end="url(#arrow)" />
  <line x1="410" y1="214" x2="540" y2="272" stroke="currentColor" stroke-opacity="0.45" stroke-dasharray="4 3" stroke-width="1.5" marker-end="url(#arrow)" />
  <rect x="110" y="30" width="100" height="36" rx="6" fill="currentColor" fill-opacity="0.07" stroke="currentColor" stroke-opacity="0.35" />
  <text x="160" y="53" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">u8 (0..255)</text>
  <rect x="110" y="110" width="100" height="36" rx="6" fill="currentColor" fill-opacity="0.07" stroke="currentColor" stroke-opacity="0.35" />
  <text x="160" y="133" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">u16</text>
  <rect x="110" y="190" width="100" height="36" rx="6" fill="currentColor" fill-opacity="0.07" stroke="currentColor" stroke-opacity="0.35" />
  <text x="160" y="213" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">u32</text>
  <rect x="110" y="270" width="100" height="36" rx="6" fill="currentColor" fill-opacity="0.07" stroke="currentColor" stroke-opacity="0.35" />
  <text x="160" y="293" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">u64 (bigint)</text>
  <rect x="310" y="30" width="100" height="36" rx="6" fill="currentColor" fill-opacity="0.07" stroke="currentColor" stroke-opacity="0.35" />
  <text x="360" y="53" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">s8 (-128..127)</text>
  <rect x="310" y="110" width="100" height="36" rx="6" fill="#3b82f6" fill-opacity="0.12" stroke="#3b82f6" stroke-width="1.5" />
  <text x="360" y="133" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">s16 (u8 ⊔ s8)</text>
  <rect x="310" y="190" width="100" height="36" rx="6" fill="#3b82f6" fill-opacity="0.12" stroke="#3b82f6" stroke-width="1.5" />
  <text x="360" y="213" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">s32 (u16 ⊔ s16)</text>
  <rect x="310" y="270" width="100" height="36" rx="6" fill="#3b82f6" fill-opacity="0.12" stroke="#3b82f6" stroke-width="1.5" />
  <text x="360" y="293" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">s64 (u32 ⊔ s32)</text>
  <rect x="545" y="190" width="175" height="36" rx="6" fill="currentColor" fill-opacity="0.07" stroke="currentColor" stroke-opacity="0.35" />
  <text x="632" y="213" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">f32 (WIT float32)</text>
  <line x1="632" y1="226" x2="632" y2="264" stroke="currentColor" stroke-opacity="0.45" stroke-width="1.5" marker-end="url(#arrow)" />
  <rect x="545" y="270" width="175" height="36" rx="6" fill="currentColor" fill-opacity="0.1" stroke="currentColor" stroke-opacity="0.5" stroke-width="1.5" />
  <text x="632" y="293" text-anchor="middle" fill="currentColor" font-size="13" font-weight="600">f64 (JS number)</text>
  <text x="110" y="340" fill="currentColor" opacity="0.75" font-size="11">Solid arrows: same-signedness widening · Blue arrows: cross-signedness join · Dashed arrows: exact promotion to IEEE 754 f64</text>
</svg>
<figcaption>Subsumption join lattice across WIT fixed-width integers and ECMAScript number representations.</figcaption>
</figure>

## Register Promotion

Evaluating intermediate integer expressions directly in narrow 8-, 16-, or 32-bit arithmetic would either wrap prematurely (violating JavaScript's 53-bit exact integer semantics) or require converting every operand to `f64` and back.

Perry-WIT resolves this in the `ProgramFacts` analysis pass:

1. **Interval Tracking**: Every SSA value carries a conservative `[min, max]` integer interval alongside its demanded bitwidth.
2. **64-Bit Intermediate Evaluation**: When an integer expression (`+`, `-`, `*`) combines narrow integer inputs (`u8`–`s32`) without a bitwise truncation mask (`| 0`, `>>> 0`), the compiler promotes intermediate computations to native WebAssembly `i64` registers. Because any product or sum of two 32-bit integers fits without overflow inside a 64-bit register, intermediate arithmetic executes in a single `i64.add`, `i64.sub`, or `i64.mul` instruction with zero heap allocation and zero premature wrap.
3. **Demand-Driven Narrowing**: When the result feeds a bitwise operator (`x | 0`), a typed array store, or a WIT return boundary, `ProgramFacts` narrows the value back to `i32` (`i32.wrap_i64`) after checking or proving that the interval lies within the target domain.

## Boundary Marshalling

At WIT component boundaries, Canonical ABI adapters enforce strict range and representation contracts:

- **32-Bit and Narrower Integers (`u8`–`u32`, `s8`–`s32`)**: Surface as `number` in generated TypeScript declarations. Inbound arguments arrive masked or sign-extended by the Canonical ABI. Outbound return values derived from unconstrained `number` computations undergo finite/integral validation and range checks before lowering to `i32`.
- **64-Bit Integers (`u64`, `s64`)**: Surface as `bigint` in generated TypeScript declarations because IEEE 754 `f64` cannot losslessly represent integers beyond `Number.MAX_SAFE_INTEGER` ($2^{53} - 1$).
  - Outbound `s64` values verify $-2^{63} \le x \le 2^{63} - 1$ and lower directly to `i64`.
  - Outbound `u64` values verify $0 \le x \le 2^{64} - 1$ and lower directly to `i64`.
  - Out-of-range `bigint` values crossing a `u64` or `s64` boundary throw a catchable `RangeError`.
- **Arbitrary-Width `bigint` Operations**: Within guest code, `bigint` supports arbitrary-precision addition (`+`), subtraction (`-`), unary negation (`-x`), relational comparisons (`<`, `<=`, `===`), and decimal formatting (`.toString()`). Multiplication (`*`), division (`/`), remainder (`%`), and `BigInt(number)` specialize to 64-bit native instructions when operands are proven within 64-bit bounds.

## Code Generation Examples

The examples below illustrate how WIT signatures and source patterns lower to WebAssembly instructions.

### Cross-Signedness Widening in `i64` Registers

Given a WIT export taking `a: u8` and `b: s8` and returning `s16`:

```wit
export combine-delta: func(base: u8, delta: s8) -> s16;
```

```ts
export function combineDelta(base: number, delta: number): number {
  return base + delta;
}
```

`ProgramFacts` types `base` as `UInt8 [0, 255]` and `delta` as `SInt8 [-128, 127]`. Their subsumption join `u8 ⊔ s8` is `SInt16 [-128, 382]`, which fits strictly inside `s16` (`[-32768, 32767]`). The compiler emits zero float conversions and elides the outbound range check:

```wat
(func $combine-delta (param $base i32) (param $delta i32) (result i32)
  local.get $base
  i32.const 255
  i32.and
  local.get $delta
  i32.extend8_s
  i32.add)
```

### Overflow-Free Intermediate `i64` Arithmetic

When two `u32` values are multiplied and then scaled before returning a `u32`:

```wit
export scale-bytes: func(count: u32, stride: u32) -> u32;
```

```ts
export function scaleBytes(count: number, stride: number): number {
  return Math.trunc((count * stride) / 1024);
}
```

Whereas 32-bit multiplication would overflow when `count * stride >= 2^32` and `f64` multiplication would lose precision above $2^{53}$, Perry-WIT promotes `count * stride` to an exact 64-bit `i64.mul` and divides by `1024` via `i64.div_u`:

```wat
(func $scale-bytes (param $count i32) (param $stride i32) (result i32)
  local.get $count
  i64.extend_i32_u
  local.get $stride
  i64.extend_i32_u
  i64.mul
  i64.const 1024
  i64.div_u
  i32.wrap_i64)
```
