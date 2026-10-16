//! Collection, property-presence and JSON-copy optimizations keep JavaScript semantics.
use super::compile;
use crate::core_program::Program;
use anyhow::Result;
use wasmtime::Val;

#[test]
fn object_property_presence_distinguishes_omitted_from_explicit_undefined() -> Result<()> {
    let mut program = Program::from_wasm(
        compile(
            r#"
interface Item { x?: number; y?: number }
export function run(query: number): number {
    const src: Item = {};
    const dst: Item = { x: src.x };
    const heapSrc: Item = {};
    const heapKeys = Object.keys(heapSrc).length;
    const heapDst: Item = { x: heapSrc.x };
    const assigned: Item = { x: 1 };
    assigned.x = src.x;
    const jsonMissing = JSON.parse<Item>("{}");
    const explicit: Item = { x: undefined };
    const values = Object.values(explicit);
    const entries = Object.entries(explicit);
    if (query === 0) return ("x" in dst) && dst.x === undefined ? 1 : 0;
    if (query === 1) return Object.keys(dst).length * 100 + heapKeys * 10 + Object.keys(heapDst).length;
    if (query === 2) return Object.keys(assigned).length * 10 + ("x" in jsonMissing ? 1 : 0);
    return values.length === 1 &&
        values[0] === undefined &&
        entries.length === 1 &&
        entries[0][0] === "x" &&
        entries[0][1] === undefined
        ? 1
        : 0;
}
"#,
            "func(query:s32)->s32",
        )?
        .core,
    );
    for (query, expected, label) in [
        (0, 1, "copied undefined property remains present with undefined value"),
        (1, 101, "Object.keys counts on stack/heap records with omitted vs copied undefined"),
        (2, 10, "assigned undefined stays present while JSON.parse('{}') omits missing field"),
        (3, 1, "Object.values and Object.entries include explicit undefined property"),
    ] {
        assert_eq!(
            program.call("run", &[Val::I32(query)]).0[0].unwrap_i32(),
            expected,
            "{label}"
        );
    }
    Ok(())
}

/// Live iterator mutation, resumption, and compaction are checked by the stateful
/// `ecma.collection.iterators` generator; this checks nested collections, iterator
/// object identity, Set spread, and internal-prefix key hygiene.
#[test]
fn collection_key_hygiene_spread_and_iterator_identity_hold() -> Result<()> {
    let mut program = Program::from_wasm(
        compile(
            r#"
export function run(query: number): number {
    if (query === 0) {
        const nested = new Map<string, Set<number>>([["key", new Set<number>([3, 4])]]);
        return (nested.get("key").has(4) ? 10 : 0) + nested.get("key").size;
    }
    if (query === 1) {
        const idSet = new Set<number>([1]);
        return (idSet.values() as unknown) !== (idSet.values() as unknown) ? 1 : 0;
    }
    if (query === 2) {
        const spreadSet = [...new Set([4, 5])];
        return spreadSet.length * 100 + spreadSet[0] * 10 + spreadSet[1];
    }
    const nulSet = new Set(["\u0000perry_set_type_args", "__perry_set_type_args", "s"]);
    return nulSet.size * 100 +
        (nulSet.has("\u0000perry_set_type_args") ? 10 : 0) +
        (nulSet.has("__perry_set_type_args") ? 1 : 0);
}
"#,
            "func(query:s32)->s32",
        )?
        .core,
    );
    for (query, expected, label) in [
        (0, 12, "nested Map<string, Set<number>> preserves inner Set membership and size"),
        (1, 1, "Set.values() allocates distinct iterator instances"),
        (2, 245, "spreading Set into array preserves order and elements"),
        (3, 311, "Set string keys with NUL or internal prefix do not collide with metadata"),
    ] {
        assert_eq!(
            program.call("run", &[Val::I32(query)]).0[0].unwrap_i32(),
            expected,
            "{label}"
        );
    }
    Ok(())
}

#[test]
fn collection_churn_reuses_memory_and_scales_linearly() -> Result<()> {
    let mut churn = Program::from_wasm(
        compile(
            r#"
export function run(n: number): number {
    const s = new Set<number>([0]);
    const m = new Map<number, number>([[0, 0]]);
    for (let i = 0; i < n; i = i + 1) {
        s.values();
        m.values();
        for (const _v of s.values()) {
            break;
        }
        for (const _v of s) {
            break;
        }
        for (const _e of m) {
            break;
        }
        s.add(i + 1);
        s.delete(i + 1);
        m.set(i + 1, i);
        m.delete(i + 1);
    }
    return s.size + m.size;
}
"#,
            "func(n:s32)->s32",
        )?
        .core,
    );
    let (res_128, fuel_128) = churn.call("run", &[Val::I32(128)]);
    let mem_128 = churn.memory_bytes();
    let (res_512, fuel_512) = churn.call("run", &[Val::I32(512)]);
    let mem_512 = churn.memory_bytes();
    let (res_64k, _) = churn.call("run", &[Val::I32(65536)]);
    let mem_64k = churn.memory_bytes();
    assert_eq!(res_128[0].unwrap_i32(), 2);
    assert_eq!(res_512[0].unwrap_i32(), 2);
    assert_eq!(res_64k[0].unwrap_i32(), 2);
    assert_eq!(mem_128, mem_512);
    assert_eq!(mem_512, mem_64k);
    assert!(
        mem_64k <= 65536,
        "expected bounded 64 KiB guest memory after 65,536 churn iterations, got {mem_64k}"
    );
    assert!(
        fuel_512 < 6 * fuel_128,
        "expected linear churn scaling, got fuel(128)={fuel_128}, fuel(512)={fuel_512}"
    );
    Ok(())
}

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
