//! JSON-copy normalization rewrites `JSON.parse(JSON.stringify(...))` and keeps its semantics.
use super::compile;
use crate::core_program::Program;
use anyhow::Result;
use wasmtime::Val;

#[test]
fn normalize_json_copy_rewrites_parse_stringify_and_preserves_edge_cases() -> Result<()> {
    let compiled = compile(
        r#"
interface Item { a: number; b?: number; tags: string[] }
export function run(x: number): number {
    const src: Item = { a: x === 0 ? -0 : x, b: undefined, tags: ["ok"] };
    const other: Item = { a: 1, tags: [] };
    const text = JSON.stringify(src);
    other.a = 99;
    const copy = JSON.parse<Item>(text);
    const mutated: Item = { a: 10, tags: [] };
    const mText = JSON.stringify(mutated);
    mutated.a = 20;
    const preserved = JSON.parse<Item>(mText);
    const negZeroFixed = Object.is(copy.a, 0) && !Object.is(copy.a, -0);
    let threw = false;
    try {
        JSON.parse<Item>(JSON.stringify({ a: NaN, tags: ["bad"] }));
    } catch (_e) {
        threw = true;
    }
    return negZeroFixed && threw && !("b" in copy) && preserved.a === 10 ? copy.tags.length : -1;
}
"#,
        "func(x:f64)->s32",
    )?;
    let report = &compiled.allocation_report;
    assert!(!report.contains("parse_stringify_rewrites=0"), "{report}");
    assert!(!report.contains("parse_stringify_missed=0"), "{report}");
    let mut program = Program::from_wasm(compiled.core);
    assert_eq!(
        program.call("run", &[Val::F64(0.0f64.to_bits())]).0[0].unwrap_i32(),
        1
    );
    Ok(())
}
