//! Collection, property-presence and JSON-copy optimizations keep JavaScript semantics.
use super::compile;
use crate::core_program::Program;
use anyhow::Result;
use wasmtime::Val;

/// Iterator resumption and exhaustion are checked against Node by the `ecma.json.command`
/// conformance case; this covers live views over mutated collections and churn cost.
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

    const s1 = new Set<number>([1, 2, 3]);
    let s1Total = 0;
    s1.forEach((v) => {
        s1.delete(v);
        s1Total = s1Total + v;
    });

    const s2 = new Set([1, 2, 3]);
    let s2Total = 0;
    for (const v of s2) {
        s2.delete(v);
        s2Total = s2Total + v;
    }

    const m1 = new Map<number, number>([[1, 10], [2, 20], [3, 30]]);
    let m1Total = 0;
    m1.forEach((v, k) => {
        m1.delete(k);
        m1Total = m1Total + v;
    });

    const m2 = new Map([[1, 10], [2, 20], [3, 30]]);
    let m2Total = 0;
    for (const [k, v] of m2) {
        m2.delete(k);
        m2Total = m2Total + v;
    }

    const nested = new Map<string, Set<number>>([["key", new Set<number>([3, 4])]]);
    const nestedOk = nested.get("key").has(4) && nested.get("key").size === 2;

    const s3 = new Set([1, 2]);
    let s3Total = 0;
    s3.forEach((v) => {
        s3Total = s3Total + v;
        if (v === 1) {
            s3.clear();
            s3.add(9);
        }
    });

    // Iterators created before a mutation observe it (Set.keys lowers like Set.values).
    const liveSet = new Set<number>([1, 2]);
    const setIter = liveSet.values();
    liveSet.delete(2);
    liveSet.add(4);
    let liveSetTotal = 0;
    for (const v of setIter) {
        liveSetTotal = liveSetTotal + v;
    }

    const liveSetE = new Set<number>([1, 2]);
    const setEntIter = liveSetE.entries();
    liveSetE.delete(2);
    liveSetE.add(4);
    let liveSetEntTotal = 0;
    for (const [k, v] of setEntIter) {
        liveSetEntTotal = liveSetEntTotal + k + v;
    }

    const liveMap = new Map<number, number>([[1, 10], [2, 20]]);
    const mapValIter = liveMap.values();
    liveMap.delete(2);
    liveMap.set(4, 40);
    let liveMapValTotal = 0;
    for (const v of mapValIter) {
        liveMapValTotal = liveMapValTotal + v;
    }

    const liveMapK = new Map<number, number>([[1, 10], [2, 20]]);
    const mapKeyIter = liveMapK.keys();
    liveMapK.delete(2);
    liveMapK.set(4, 40);
    let liveMapKeyTotal = 0;
    for (const k of mapKeyIter) {
        liveMapKeyTotal = liveMapKeyTotal + k;
    }

    const liveMapE = new Map<number, number>([[1, 10], [2, 20]]);
    const mapEntIter = liveMapE.entries();
    liveMapE.delete(2);
    liveMapE.set(4, 40);
    let liveMapEntTotal = 0;
    for (const [k, v] of mapEntIter) {
        liveMapEntTotal = liveMapEntTotal + k + v;
    }

    const idSet = new Set<number>([1]);
    const iterIdentity = (idSet.values() as unknown) !== (idSet.values() as unknown);

    const spreadSet = [...new Set([4, 5])];
    const spreadOk = spreadSet.length === 2 && spreadSet[0] + spreadSet[1] === 9;

    const nulSet = new Set(["\u0000perry_set_type_args", "__perry_set_type_args", "s"]);
    const nulHygiene = nulSet.size === 3 && nulSet.has("\u0000perry_set_type_args") && nulSet.has("__perry_set_type_args");

    return copiedPresence &&
        enumPresence &&
        s1Total === 6 &&
        s2Total === 6 &&
        m1Total === 60 &&
        m2Total === 60 &&
        nestedOk &&
        s3Total === 10 &&
        liveSetTotal === 5 &&
        liveSetEntTotal === 10 &&
        liveMapValTotal === 50 &&
        liveMapKeyTotal === 5 &&
        liveMapEntTotal === 55 &&
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
    const s = new Set<number>();
    const m = new Map<number, number>();
    for (let i = 0; i < n; i = i + 1) {
        s.add(i);
        s.delete(i);
        m.set(i, i);
        m.delete(i);
    }
    return s.size + m.size;
}
"#,
            "func(n:s32)->s32",
        )?
        .core,
    );
    let (res_128, fuel_128) = churn.call("run", &[Val::I32(128)]);
    let (res_512, fuel_512) = churn.call("run", &[Val::I32(512)]);
    assert_eq!(res_128[0].unwrap_i32(), 0);
    assert_eq!(res_512[0].unwrap_i32(), 0);
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
