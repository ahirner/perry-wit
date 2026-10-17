//! WIT integer domains: typed arithmetic, wrap points, widening and exact 64-bit values.
use super::{compile, inspect_wasm};
use crate::core_program::Program;
use anyhow::Result;
use wasmparser::{Operator, Parser, Payload};
use wasmtime::Val;

#[test]
fn wit_integer_addition_uses_typed_arithmetic_without_runtime() -> Result<()> {
    for (wit, source_type, a, b, expected) in [
        ("s8", "number", 127, 1, -128),
        ("u8", "number", 255, 1, 0),
        ("s16", "number", 32767, 1, -32768),
        ("u16", "number", 65535, 1, 0),
        ("s32", "number", i32::MAX as i64, 1, i32::MIN as i64),
        ("u32", "number", u32::MAX as i64, 1, 0),
        ("s64", "bigint", i64::MAX, 1, i64::MIN),
        ("u64", "bigint", -1, 1, 0),
        ("s64", "bigint", 9007199254740993, 2, 9007199254740995),
    ] {
        let compiled = compile(
            &format!(
                "export function run(a:{source_type}, b:{source_type}):{source_type} {{ return a+b; }}"
            ),
            &format!("func(a:{wit},b:{wit})->{wit}"),
        )?;
        let mut stripped = wasm_encoder::Module::new();
        for payload in Parser::new(0).parse_all(&compiled.core) {
            let payload = payload?;
            if !matches!(payload, Payload::CustomSection(_))
                && let Some((id, range)) = payload.as_section()
            {
                stripped.section(&wasm_encoder::RawSection {
                    id,
                    data: &compiled.core[range.start as usize..range.end as usize],
                });
            }
        }
        let stripped = stripped.finish();
        assert!(stripped.len() < compiled.core.len());
        let core_summary = inspect_wasm(&compiled.core)?;
        assert!(core_summary.has_name_section);
        let stripped_summary = inspect_wasm(&stripped)?;
        let wide = wit.ends_with("64");
        let mut addition = false;
        for summary in [&core_summary, &stripped_summary] {
            assert_eq!(
                summary.functions,
                if wit.ends_with("8") || wit.ends_with("16") {
                    2
                } else {
                    1
                },
                "{wit}"
            );
            assert_eq!(summary.memories, 0, "{wit}");
            assert_eq!(summary.imports, 0, "{wit}");
            assert_eq!(summary.exports, ["run"], "{wit}");
            if wit == "s32" {
                assert!(
                    matches!(
                        summary.bodies[0].as_slice(),
                        [
                            Operator::LocalGet { local_index: 0 },
                            Operator::LocalGet { local_index: 1 },
                            Operator::I32Add,
                            Operator::End,
                        ]
                    ),
                    "{:?}",
                    summary.bodies[0]
                );
            }
            let mut sign_extensions = 0;
            for op in summary.instructions() {
                addition |= matches!(
                    (op, wide),
                    (Operator::I64Add, true) | (Operator::I32Add, false)
                );
                match op {
                    Operator::I32Extend8S if wit == "s8" => sign_extensions += 1,
                    Operator::I32Extend16S if wit == "s16" => sign_extensions += 1,
                    Operator::I32Shl | Operator::I32ShrS => {
                        panic!("{wit}: use sign-extension instructions")
                    }
                    Operator::Call { .. } if matches!(wit, "s32" | "u32" | "s64" | "u64") => {
                        panic!("{wit}: exported arithmetic must not forward")
                    }
                    _ => {}
                }
                assert!(!format!("{op:?}").contains("F64"), "{wit}: {op:?}");
            }
            if matches!(wit, "s8" | "s16") {
                assert_eq!(sign_extensions, 3, "{wit}: a+b sign extensions");
            }
        }
        assert!(addition, "{wit}");
        assert!(compiled.core.len() < 2963, "{wit}");
        let mut program = Program::from_wasm(compiled.core);
        let args = if wide {
            [Val::I64(a), Val::I64(b)]
        } else {
            [Val::I32(a as i32), Val::I32(b as i32)]
        };
        let result = program.call("run", &args).0[0];
        assert_eq!(
            if wide {
                result.unwrap_i64()
            } else {
                result.unwrap_i32() as i64
            },
            expected,
            "{wit}"
        );
    }
    Ok(())
}

#[test]
fn integer_comparisons_preserve_literal_ranges() -> Result<()> {
    for expression in [
        "value < 256",
        "value > -1",
        "value < (128+128)",
        "value !== 511",
    ] {
        let compiled = compile(
            &format!("export function run(value:number):boolean {{ return {expression}; }}"),
            "func(value:u8)->bool",
        )?;
        let engine = wasmtime::Engine::default();
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(u8,), (bool,)>(&mut store, "run")?;
        for value in [0, 1, 254, 255] {
            assert_eq!(
                run.call(&mut store, (value,))?,
                (true,),
                "{expression}: {value}"
            );
        }
    }
    Ok(())
}

#[test]
fn mixed_domains_and_unconstrained_integer_results_widen() -> Result<()> {
    let compiled = compile(
        "export function run(a:number,b:number):number { return a+b; }",
        "func(a:s8,b:u8)->s16",
    )?;
    let mut program = Program::from_wasm(compiled.core);
    assert_eq!(
        program.call("run", &[Val::I32(-1), Val::I32(255)]).0[0].unwrap_i32(),
        254
    );
    for (source, expected) in [
        (
            "export function run(a:number,b:number):number { return a+b; }",
            8589934590.0,
        ),
        (
            "function add(a:number,b:number):number { return a+b; } export function run(a:number,b:number):number { return add(a+b,b); }",
            12884901885.0,
        ),
    ] {
        let compiled = compile(source, "func(a:u32,b:u32)->f64")?;
        let mut program = Program::from_wasm(compiled.core);
        assert_eq!(
            program.call("run", &[Val::I32(-1), Val::I32(-1)]).0[0].unwrap_f64(),
            expected,
            "{source}"
        );
    }
    let compiled = compile(
        "export function run(a:bigint,b:bigint):bigint { return a+b; }",
        "func(a:s64,b:u64)->u64",
    )?;
    let mut program = Program::from_wasm(compiled.core);
    assert_eq!(
        program.call("run", &[Val::I64(-1), Val::I64(-1)]).0[0].unwrap_i64(),
        -2
    );
    let compiled = compile(
        "export function run(value:bigint):bigint { return -value; }",
        "func(value:u64)->u64",
    )?;
    let mut program = Program::from_wasm(compiled.core);
    assert_eq!(program.call("run", &[Val::I64(1)]).0[0].unwrap_i64(), -1);
    Ok(())
}

#[test]
fn signed_narrow_normalization_occurs_only_at_required_wrap_points() -> Result<()> {
    for wit in ["s8", "s16"] {
        let compiled = compile(
            "export function run(a:number):number { return a; }",
            &format!("func(a:{wit})->{wit}"),
        )?;
        let summary = inspect_wasm(&compiled.core)?;
        let mut extensions = 0;
        for op in summary.instructions() {
            match op {
                Operator::I32Extend8S if wit == "s8" => extensions += 1,
                Operator::I32Extend16S if wit == "s16" => extensions += 1,
                Operator::I32Shl | Operator::I32ShrS => panic!("use sign extension"),
                _ => {}
            }
        }
        assert_eq!(extensions, 1, "{wit}: a");
    }
    Ok(())
}

#[test]
fn perry_types_and_wit_sdk_integer_aliases_enforce_wrapping_and_width_widening() -> Result<()> {
    let wit_src = r#"
        package test:optimization;
        interface math { type byte = u8; }
        world test {
            import math;
            export wrap-byte: func(a: u32, b: u32) -> u32;
            export widen-s8: func(a: s8, b: s8) -> s32;
            export big: func(a: u64, b: u64) -> u64;
        }
    "#;
    let (resolve, package) =
        perry_wit::component::wit::resolve_source("optimization.wit", wit_src)?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let source = r#"
        import type { s8, s32, u64 } from "perry:types";
        import type { Byte } from "wit:test:optimization/math";
        function widenAfterAdd(a: s8, b: s8): s32 { const sum: s8 = (a + b) as s8; return sum; }
        export function wrapByte(a: number, b: number): number { const wrapped: Byte = (a + b) as Byte; return wrapped; }
        export function widenS8(a: s8, b: s8): s32 { return widenAfterAdd(a, b); }
        export function big(a: u64, b: u64): u64 { return a + b; }
    "#;
    let compiled = perry_wit::compile_for_world(source, "optimization.ts", &resolve, world, true)?;
    let mut program = Program::from_wasm(compiled.core);
    assert_eq!(program.memory_bytes(), 0);
    assert_eq!(
        program.call("wrap-byte", &[Val::I32(250), Val::I32(10)]).0[0].unwrap_i32(),
        4
    );
    assert_eq!(
        program.call("widen-s8", &[Val::I32(127), Val::I32(1)]).0[0].unwrap_i32(),
        -128
    );
    assert_eq!(
        program
            .call("big", &[Val::I64(9007199254740992), Val::I64(1)])
            .0[0]
            .unwrap_i64(),
        9007199254740993
    );
    Ok(())
}

#[test]
fn exact_64_bit_integers_and_parse_int_preserve_precision() -> Result<()> {
    let mut roundtrip = Program::from_wasm(
        compile(
            r#"
import type { u64, s64 } from "perry:types";
interface Packet { id: u64; delta: s64 }
type ArithRec = { v: number; opt?: number };
function id(x: u64): u64 { return x; }
function makeArith(x: number): ArithRec { return { v: x * 2, opt: x * 3 }; }
export function run(rawId: u64, delta: s64, u: number): number {
    const input: Packet = { id: id(rawId), delta };
    const parsed = JSON.parse<Packet>(JSON.stringify(input));
    const rec: ArithRec = { v: u * 2, opt: u * 3 };
    const arr: number[] = [u * 2];
    const viaHelper = makeArith(u);
    const arithOk =
        JSON.stringify(rec.v) === "6000000000" &&
        JSON.stringify(rec.opt ?? 0) === "9000000000" &&
        JSON.stringify(arr[0]) === "6000000000" &&
        JSON.stringify(viaHelper.v) === "6000000000";
    const ok =
        arithOk &&
        JSON.stringify(id(rawId)) === "9007199254740993" &&
        JSON.stringify(parsed.id) === "9007199254740993";
    return ok && parsed.id === rawId && parsed.delta === delta ? 1 : 0;
}
"#,
            "func(raw-id:u64,delta:s64,u:u32)->s32",
        )?
        .core,
    );
    let (res, _) = roundtrip.call(
        "run",
        &[
            Val::I64(9007199254740993),
            Val::I64(-9007199254740993),
            Val::I32(3_000_000_000u32 as i32),
        ],
    );
    assert_eq!(res[0].unwrap_i32(), 1);

    // Aggregate specialization across nested/recursive records, arrays, optionals,
    // permutations, and origin summarization is tested by `ecma.aggregate.specialization`;
    // this checks parseInt lexical edge cases, 64-bit remainder, Math intrinsics,
    // exact64 aggregate JSON serialization, and reference alias identity.
    let mut parse_int = Program::from_wasm(
        compile(
            r#"
interface Box { raw: string }
interface Cell { value: number }
interface Nested { inner: Cell; list: number[] }
interface Node { value: number; next?: Node }
function fwd(v: number): number { return v; }
function choose(flag: number): string { return flag > 0 ? "9007199254740993" : "7"; }
function chooseU64Odd(flag: number): string { return flag > 0 ? "18446744073709551615" : "7"; }
function chooseU64Even(flag: number): string { return flag > 0 ? "18446744073709551614" : "6"; }
function radix(flag: number): number { return flag > 0 ? 10 : 16; }
export function run(flag: number): number {
    const s = choose(flag);
    const a = parseInt("9007199254740993");
    const b = parseInt("9007199254740993");
    const eq = a === b && !(a !== b);
    const mul = JSON.stringify(a * 2) === "18014398509481986";
    const cmp = a < 10000000000000000 && !(a > 10000000000000000);
    const helper = fwd(a) === b && JSON.stringify(fwd(a)) === "9007199254740993";
    const overflow = Number.isNaN(parseInt("18446744073709551616"));
    const radixWrap = parseInt("10", 4294967298) === 2;
    const runtimeText = JSON.stringify(parseInt(s)) === "9007199254740993";
    const runtimeRadix = JSON.stringify(parseInt("9007199254740993", radix(flag))) === "9007199254740993";
    const box = JSON.parse<Box>(flag > 0 ? '{"raw":"9007199254740993"}' : '{"raw":"7"}');
    const runtimeJson = JSON.stringify(parseInt(box.raw)) === "9007199254740993";
    const nonJsWs = Number.isNaN(parseInt("\u008510"));
    const u64OddRem = parseInt(chooseU64Odd(flag)) % 2;
    const u64EvenRem = parseInt(chooseU64Even(flag)) % 2;
    const numRem =
        (parseInt(s) % 10) === 3 &&
        JSON.stringify(u64OddRem) === "1" &&
        u64EvenRem === 0 &&
        !Object.is(u64EvenRem, -0);
    const numMath =
        JSON.stringify(Math.trunc(parseInt(s)) + 1) === "9007199254740994" &&
        JSON.stringify(Math.abs(parseInt("-9007199254740993")) + 1) === "9007199254740994" &&
        JSON.stringify(Math.max(parseInt(s), 10) + 1) === "9007199254740994";
    const exactVal = parseInt(s);
    const recJson = JSON.stringify({ value: exactVal }) === '{"value":9007199254740993}';
    const nestedObj: Nested = { inner: { value: exactVal }, list: [exactVal, 1.5] };
    const nestedJson =
        JSON.stringify(nestedObj) === '{"inner":{"value":9007199254740993},"list":[9007199254740993,1.5]}' &&
        JSON.stringify([...nestedObj.list]) === "[9007199254740993,1.5]";
    const optNode: Node = { value: exactVal };
    const optJson = JSON.stringify(optNode) === '{"value":9007199254740993}';
    const aliasSelf: Node = { value: exactVal };
    aliasSelf.next = aliasSelf;
    const sharedCell: Cell = { value: exactVal };
    const sharedPair: Cell[] = [sharedCell].concat([sharedCell]);
    sharedPair[0].value = 1.5;
    const aliasIdentity =
        aliasSelf.next === aliasSelf &&
        (aliasSelf.next?.value ?? 0) === exactVal &&
        sharedPair[1].value === 1.5;
    return eq && mul && cmp && helper && overflow && radixWrap && runtimeText && runtimeRadix && runtimeJson && nonJsWs && numRem && numMath && recJson && nestedJson && optJson && aliasIdentity ? 1 : 0;
}
"#,
            "func(flag:s32)->s32",
        )?
        .core,
    );
    assert_eq!(parse_int.call("run", &[Val::I32(1)]).0[0].unwrap_i32(), 1);

    // Specialized aggregate results (records and lists with perry.numeric fields/elements) lower cleanly across WIT boundaries.
    let wit_src = r#"
        package test:numeric-boundary;
        world test {
            record pair { value: f64, name: string }
            record single-f64 { value: f64 }
            record single-s32 { value: s32 }
            record single-s64 { value: s64 }
            export run-pair: func(flag: s32) -> pair;
            export run-f64: func(flag: s32) -> single-f64;
            export run-s32: func(flag: s32) -> single-s32;
            export run-s64: func(flag: s32) -> single-s64;
            export run-list: func(flag: s32) -> list<f64>;
        }
    "#;
    let (resolve, package) =
        perry_wit::component::wit::resolve_source("numeric-boundary.wit", wit_src)?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let boundary_source = r#"
        import type { s64 } from "perry:types";
        interface Pair { value: number; name: string }
        interface SingleNum { value: number }
        interface SingleS64 { value: s64 }
        function choose(flag: number): string { return flag > 0 ? "42" : "7"; }
        export function runPair(flag: number): Pair { return { value: parseInt(choose(flag)), name: "a" }; }
        export function runF64(flag: number): SingleNum { return { value: parseInt(choose(flag)) }; }
        export function runS32(flag: number): SingleNum { return { value: parseInt(choose(flag)) }; }
        export function runS64(flag: number): SingleS64 { return { value: 9007199254740993n as s64 }; }
        export function runList(flag: number): number[] { return [parseInt(choose(flag)), 2.5]; }
    "#;
    let boundary_compiled = perry_wit::compile_for_world(
        boundary_source,
        "numeric-boundary.ts",
        &resolve,
        world,
        true,
    )?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) =
        crate::world::instantiate(&engine, &boundary_compiled.component.unwrap().stripped)?;
    use wasmtime::component::Val as CVal;
    let mut out = [CVal::Bool(false)];
    instance.get_func(&mut store, "run-pair").unwrap().call(
        &mut store,
        &[CVal::S32(1)],
        &mut out,
    )?;
    assert_eq!(
        out[0],
        CVal::Record(vec![
            ("value".into(), CVal::Float64(42.0)),
            ("name".into(), CVal::String("a".into())),
        ])
    );
    let mut out = [CVal::Bool(false)];
    instance.get_func(&mut store, "run-f64").unwrap().call(
        &mut store,
        &[CVal::S32(1)],
        &mut out,
    )?;
    assert_eq!(
        out[0],
        CVal::Record(vec![("value".into(), CVal::Float64(42.0))])
    );
    let mut out = [CVal::Bool(false)];
    instance.get_func(&mut store, "run-s32").unwrap().call(
        &mut store,
        &[CVal::S32(1)],
        &mut out,
    )?;
    assert_eq!(out[0], CVal::Record(vec![("value".into(), CVal::S32(42))]));
    let mut out = [CVal::Bool(false)];
    instance.get_func(&mut store, "run-s64").unwrap().call(
        &mut store,
        &[CVal::S32(1)],
        &mut out,
    )?;
    assert_eq!(
        out[0],
        CVal::Record(vec![("value".into(), CVal::S64(9007199254740993))])
    );
    let run_list = instance.get_typed_func::<(i32,), (Vec<f64>,)>(&mut store, "run-list")?;
    assert_eq!(run_list.call(&mut store, (1,))?, (vec![42.0, 2.5],));
    Ok(())
}
