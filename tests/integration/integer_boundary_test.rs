//! Numbers returned as WIT integers convert like JavaScript typed-array stores (ECMAScript
//! `ToUint8` through `ToInt32`). Expected values are Node's, e.g. `new Uint8Array([x])[0]`.
use crate::world::Compiler::Standard;
use anyhow::{Context, Result};
use wasmtime::{Engine, component::Val};

const WIT: &str = "package test:integers;
world integers {
  export to-u8: func(x: f64) -> u8;
  export to-s8: func(x: f64) -> s8;
  export to-u32: func(x: f64) -> u32;
  export to-s32: func(x: f64) -> s32;
  export scale8: func(a: u8, b: u8) -> u8;
  export add32: func(a: u32, b: u32) -> u32;
  export literal: func(flag: bool) -> s32;
  export cap: func(a: u8) -> u8;
  export counter: func(n: u8) -> u8;
}";

const SOURCE: &str = "
export function toU8(x: number): number { return x; }
export function toS8(x: number): number { return x; }
export function toU32(x: number): number { return x; }
export function toS32(x: number): number { return x; }
export function scale8(a: number, b: number): number { return a * b; }
export function add32(a: number, b: number): number { return a + b; }
export function literal(flag: boolean): number {
  let v = -5;
  if (flag) v = 3000000000;
  return v + 0.5 > 0 ? v : 7;
}
export function cap(a: number): number {
  const sum = a + 100;
  return sum > 255 ? 1 : sum;
}
export function counter(n: number): number {
  let i = 0;
  while (i < n) i = i + 100;
  return i;
}
";

#[test]
fn number_results_wrap_like_javascript() -> Result<()> {
    let engine = Engine::default();
    let bytes = Standard.component(WIT, "integers", "integers.ts", SOURCE)?;
    let (mut store, instance) = crate::world::instantiate(&engine, &bytes)?;
    let mut call = |name: &str, args: &[Val]| -> Result<Val> {
        let func = instance
            .get_func(&mut store, name)
            .with_context(|| format!("export {name}"))?;
        let mut result = [Val::Bool(false)];
        func.call(&mut store, args, &mut result)?;
        let [value] = result;
        Ok(value)
    };
    for (name, x, expected) in [
        // Truncate the fraction, then keep the low 8 bits.
        ("to-u8", 300.7, Val::U8(44)),
        ("to-u8", -1.0, Val::U8(255)),
        ("to-s8", 200.0, Val::S8(-56)),
        // Beyond 2^32 and beyond 2^63 the reduction stays exact.
        ("to-u32", 1e20, Val::U32(1_661_992_960)),
        ("to-u32", -1e20, Val::U32(2_632_974_336)),
        ("to-s32", 18_446_744_073_709_551_616.0, Val::S32(0)),
        ("to-s32", 3e9, Val::S32(-1_294_967_296)),
        ("to-u32", f64::NAN, Val::U32(0)),
        ("to-s32", f64::INFINITY, Val::S32(0)),
    ] {
        assert_eq!(call(name, &[Val::Float64(x)])?, expected, "{name}({x})");
    }
    // Integer arithmetic wraps the same way.
    assert_eq!(call("scale8", &[Val::U8(200), Val::U8(2)])?, Val::U8(144));
    assert_eq!(
        call("add32", &[Val::U32(u32::MAX), Val::U32(1)])?,
        Val::U32(0)
    );
    // A literal that does not fit the returned width keeps its value until the return.
    assert_eq!(
        call("literal", &[Val::Bool(true)])?,
        Val::S32(-1_294_967_296)
    );
    assert_eq!(call("literal", &[Val::Bool(false)])?, Val::S32(7));
    // A narrow return type does not narrow intermediate values or loop counters.
    assert_eq!(call("cap", &[Val::U8(200)])?, Val::U8(1));
    assert_eq!(call("counter", &[Val::U8(250)])?, Val::U8(44));
    Ok(())
}
