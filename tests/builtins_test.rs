//! Built-in services against values recorded from Node.js 22 (`node --experimental-strip-types`):
//! `Math`, `Date`, string methods with UTF-16 indices and Unicode case mapping,
//! `Uint8Array`, `TextEncoder`/`TextDecoder` and `JSON`.
use perry_codegen::status_code;
use perry_mir::attrs::ErrorKind;
use wasmtime::{Engine, Instance, Module, Store, Val};

const SOURCE: &str = r#"
function identity(value: number): number { return value; }
export function math(x: number): number {
  return Math.floor(x) * 1e8 + Math.ceil(x) * 1e6 + Math.trunc(x) * 1e4 + Math.abs(x) * 100 + Math.round(x) + Math.min(x, 3) * 1e-2 + Math.max(x, -1) * 1e-3;
}
export function roundSign(x: number): number { return 1 / Math.round(x); }
export function dates(x: number): number { return new Date(x).getTime(); }
export function iso(x: number): number { return new Date(x).toISOString().length + new Date(x).toISOString().charCodeAt(5); }
export function strings(x: number): number {
  const s = "é漢abc  ";
  return s.charAt(x).length * 1e9 + s.indexOf("a", x) * 1e7 + s.slice(-4, x).length * 1e5 + s.substring(x, 1).length * 1e4
    + s.toUpperCase().charCodeAt(0) + s.trim().length * 1e-1 + s.lastIndexOf("b") * 1e-3 + (s.includes("漢") ? 1e-5 : 0);
}
export function cases(x: number): number { return ("Straße" + x).toUpperCase().length * 100 + "ÀÉ".toLowerCase().charCodeAt(1); }
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
export function encoded(x: number): number {
  return new TextEncoder().encode("é漢a").subarray(x, 8).byteLength;
}
export function decoded(x: number): number {
  const input = new Uint8Array([0xe6, 0xbc, 0xa2, 0x41, 0xff, x]);
  const text = new TextDecoder().decode(input);
  let fatal = 0;
  try { new TextDecoder("utf-8", { fatal: true }).decode(input); } catch (e) { fatal = 1; }
  return text.length * 10 + fatal + text.charCodeAt(2) * 1e-5;
}
export function json(x: number): number {
  const text = JSON.stringify({ text: "a\"\n\u0001é", value: x, list: [1, x, -0], flag: true });
  const parsed = JSON.parse(text);
  return identity(parsed.text.length) * 1000 + text.length + identity(parsed.list[1]) * 1e-3 + identity(JSON.parse([JSON.stringify({ text: "漢" })][0]).text.length) * 1e-6;
}
export function badJson(x: number): number {
  try { JSON.parse("{" + x); return 0; } catch (e) { return 1; }
}
"#;

const INPUTS: [f64; 7] = [0.0, 1.0, -0.5, 2.5, -2.5, 7.0, f64::NAN];

#[test]
fn builtins_match_node() {
    let wasm = perry_wit::compile_core(SOURCE, "builtins.ts").unwrap_or_else(|e| panic!("{e:#}"));
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());
    let module = Module::new(&engine, &wasm).unwrap();
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    let expected: [(&str, [Result<f64, &str>; 7]); 11] = [
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
            "roundSign",
            [
                Ok(f64::INFINITY),
                Ok(1.0),
                Ok(f64::NEG_INFINITY),
                Ok(0.3333333333333333),
                Ok(-0.5),
                Ok(0.14285714285714285),
                Ok(f64::NAN),
            ],
        ),
        (
            "dates",
            [
                Ok(0.0),
                Ok(1.0),
                Ok(0.0),
                Ok(2.0),
                Ok(-2.0),
                Ok(7.0),
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
                Err("RangeError"),
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
            "cases",
            [
                Ok(1033.0),
                Ok(1033.0),
                Ok(1333.0),
                Ok(1233.0),
                Ok(1333.0),
                Ok(1033.0),
                Ok(1233.0),
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
            "encoded",
            [
                Ok(6.0),
                Ok(5.0),
                Ok(6.0),
                Ok(4.0),
                Ok(2.0),
                Ok(0.0),
                Ok(6.0),
            ],
        ),
        (
            "decoded",
            [
                Ok(41.65533),
                Ok(41.65533),
                Ok(41.65533),
                Ok(41.65533),
                Ok(41.65533),
                Ok(41.65533),
                Ok(41.65533),
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
        let function = instance.get_func(&mut store, name).unwrap();
        for (x, outcome) in INPUTS.iter().zip(outcomes) {
            let mut results = vec![Val::I32(0); function.ty(&store).results().len()];
            function
                .call(&mut store, &[Val::F64(x.to_bits())], &mut results)
                .unwrap();
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
                Err(error) => {
                    let kind = match error {
                        "RangeError" => ErrorKind::RangeError,
                        "TypeError" => ErrorKind::TypeError,
                        _ => ErrorKind::Any,
                    };
                    assert_eq!(
                        status,
                        status_code(kind),
                        "{name}({x}) should throw {error}"
                    );
                }
            }
        }
    }
}
