//! Collection, property-presence and JSON-copy optimizations keep JavaScript semantics.
use super::compile;
use crate::core_program::Program;
use anyhow::Result;
use wasmtime::Val;

/// Live iterator mutation, resumption, and compaction are checked by the stateful
/// `ecma.collection.iterators` generator; this covers property presence, key hygiene,
/// iterator identity, and linear churn cost.
#[test]
fn collection_mutation_during_iteration_and_property_presence_preserve_semantics() -> Result<()> {
    let mut program = Program::from_wasm(
        compile(
            r#"
interface Item { x?: number; y?: number }
export function run(): number {
    const src: Item = {};
    const dst: Item = { x: src.x };
    const heapSrc: Item = {};
    const heapKeys = Object.keys(heapSrc).length;
    const heapDst: Item = { x: heapSrc.x };
    const assigned: Item = { x: 1 };
    assigned.x = src.x;
    const jsonMissing = JSON.parse<Item>("{}");
    const copiedPresence =
        ("x" in dst) &&
        dst.x === undefined &&
        Object.keys(dst).length === 1 &&
        heapKeys === 0 &&
        Object.keys(heapDst).length === 1 &&
        Object.keys(assigned).length === 1 &&
        !("x" in jsonMissing);

    const explicit: Item = { x: undefined };
    const values = Object.values(explicit);
    const entries = Object.entries(explicit);
    const enumPresence =
        values.length === 1 &&
        values[0] === undefined &&
        entries.length === 1 &&
        entries[0][0] === "x" &&
        entries[0][1] === undefined;

    const nested = new Map<string, Set<number>>([["key", new Set<number>([3, 4])]]);
    const nestedOk = nested.get("key").has(4) && nested.get("key").size === 2;

    const idSet = new Set<number>([1]);
    const iterIdentity = (idSet.values() as unknown) !== (idSet.values() as unknown);

    const spreadSet = [...new Set([4, 5])];
    const spreadOk = spreadSet.length === 2 && spreadSet[0] + spreadSet[1] === 9;

    const nulSet = new Set(["\u0000perry_set_type_args", "__perry_set_type_args", "s"]);
    const nulHygiene = nulSet.size === 3 && nulSet.has("\u0000perry_set_type_args") && nulSet.has("__perry_set_type_args");

    return copiedPresence &&
        enumPresence &&
        nestedOk &&
        iterIdentity &&
        spreadOk &&
        nulHygiene
        ? 1
        : 0;
}
"#,
            "func()->s32",
        )?
        .core,
    );
    assert_eq!(program.call("run", &[]).0[0].unwrap_i32(), 1);

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
