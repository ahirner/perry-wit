//! Built-in services against values recorded from Node.js 22 (`node --experimental-strip-types`):
//! `Math` (including `min`/`max`), `toISOString`, string search, trimming and slicing over BMP
//! scalar indices, `Uint8Array` copies and `JSON` escapes. Rounding, epoch reads, encoded
//! subviews and case mapping are conformance contracts or Node differential tests elsewhere.
use perry_codegen::status_code;
use perry_mir::attrs::ErrorKind;
use wasmtime::Val;

use crate::core_program::Program;

const SOURCE: &str = r#"
function identity(value: number): number { return value; }
export function math(x: number): number {
  return Math.floor(x) * 1e8 + Math.ceil(x) * 1e6 + Math.trunc(x) * 1e4 + Math.abs(x) * 100 + Math.round(x) + Math.min(x, 3) * 1e-2 + Math.max(x, -1) * 1e-3;
}
export function iso(x: number): number { return new Date(x).toISOString().length + new Date(x).toISOString().codePointAt(5); }
export function strings(x: number): number {
  const s = "é漢abc  ";
  return s.charAt(x).length * 1e9 + s.indexOf("a", x) * 1e7 + s.slice(-4, x).length * 1e5 + s.substring(x, 1).length * 1e4
    + s.toUpperCase().codePointAt(0) + s.trim().length * 1e-1 + s.lastIndexOf("b") * 1e-3 + (s.includes("漢") ? 1e-5 : 0);
}
export function bytes(x: number): number {
  const b = new Uint8Array([x, 256 + x, -1, 1.7]);
  const sub = b.subarray(1, 3);
  sub[0] = 300;
  const copy = new Uint8Array(b);
  copy.set(sub, 2);
  let total = 0;
  for (let i = 0; i < copy.length; i++) { total = total * 1000 + (copy[i] as number); }
  return total + (b[9] === undefined ? 0.5 : 0) + sub.byteLength * 1e-2;
}
export function json(x: number): number {
  const text = JSON.stringify({ text: "a\"\n\u0001é", value: x, list: [1, x, -0], flag: true });
  const parsed = JSON.parse(text);
  const item = parsed.list[1] === null ? 0 : identity(parsed.list[1]);
  return identity(parsed.text.length) * 1000 + text.length + item * 1e-3 + identity(JSON.parse([JSON.stringify({ text: "漢" })][0]).text.length) * 1e-6;
}
export function badJson(x: number): number {
  try { JSON.parse("{" + x); return 0; } catch (e) { return 1; }
}
"#;

const INPUTS: [f64; 7] = [0.0, 1.0, -0.5, 2.5, -2.5, 7.0, f64::NAN];

#[test]
fn builtins_match_node() {
    let mut program = Program::new(SOURCE);
    let expected: [(&str, [Result<f64, ErrorKind>; 7]); 6] = [
        (
            "math",
            [
                Ok(0.0),
                Ok(101010101.011),
                Ok(-99999950.00549999),
                Ok(203020253.0275),
                Ok(-302019752.02599996),
                Ok(707070707.037),
                Ok(f64::NAN),
            ],
        ),
        (
            "iso",
            [
                Ok(72.0),
                Ok(72.0),
                Ok(72.0),
                Ok(72.0),
                Ok(73.0),
                Ok(72.0),
                Err(ErrorKind::RangeError),
            ],
        ),
        (
            "strings",
            [
                Ok(1020010201.50301),
                Ok(1020000201.50301),
                Ok(1020010201.50301),
                Ok(1020010201.50301),
                Ok(20210201.503009997),
                Ok(-9539798.496989999),
                Ok(1020010201.50301),
            ],
        ),
        (
            "bytes",
            [
                Ok(44044255.52),
                Ok(1044044255.52),
                Ok(44044255.52),
                Ok(2044044255.52),
                Ok(254044044255.52),
                Ok(7044044255.52),
                Ok(44044255.52),
            ],
        ),
        (
            "json",
            [
                Ok(5060.000001),
                Ok(5060.0010010000005),
                Ok(5065.999501),
                Ok(5064.002501),
                Ok(5065.997501000001),
                Ok(5060.007001),
                Ok(5066.000001),
            ],
        ),
        (
            "badJson",
            [
                Ok(1.0),
                Ok(1.0),
                Ok(1.0),
                Ok(1.0),
                Ok(1.0),
                Ok(1.0),
                Ok(1.0),
            ],
        ),
    ];
    for (name, outcomes) in expected {
        for (x, outcome) in INPUTS.iter().zip(outcomes) {
            let (results, _) = program.call(name, &[Val::F64(x.to_bits())]);
            let value = results[0].unwrap_f64();
            let status = results.get(1).map_or(0, Val::unwrap_i32);
            match outcome {
                Ok(expected) => {
                    assert_eq!(status, 0, "{name}({x}) threw");
                    assert!(
                        value.to_bits() == expected.to_bits()
                            || (value.is_nan() && expected.is_nan()),
                        "{name}({x}) = {value}, Node: {expected}"
                    );
                }
                Err(kind) => assert_eq!(
                    status,
                    status_code(kind),
                    "{name}({x}) should throw {kind:?}"
                ),
            }
        }
    }
}
