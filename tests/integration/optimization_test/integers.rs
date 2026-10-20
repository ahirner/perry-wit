//! WIT integer domains: typed arithmetic, wrap points, widening and exact 64-bit values.
use super::{compile, inspect_wasm};
use crate::core_program::Program;
use crate::world::Compiler::Standard;
use anyhow::Result;
use wasmparser::{Operator, Parser, Payload};
use wasmtime::{Val, component::Val as CVal};

#[test]
fn wit_integer_addition_uses_typed_arithmetic_without_runtime() -> Result<()> {
    for (wit, source_type, a, b, expected) in [
        ("s8", "number", 127, 1, -128),
        ("u8", "number", 255, 1, 0),
        ("s16", "number", 32767, 1, -32768),
        ("u16", "number", 65535, 1, 0),
        ("s32", "number", i32::MAX as i64, 1, i32::MIN as i64),
        ("u32", "number", u32::MAX as i64, 1, 0),
        ("s64", "s64", i64::MAX, 1, i64::MIN),
        ("u64", "u64", -1, 1, 0),
        ("s64", "s64", 9007199254740993, 2, 9007199254740995),
    ] {
        let imports = if wit.ends_with("64") {
            "import type { s64, u64 } from 'perry:types';"
        } else {
            ""
        };
        let compiled = compile(
            &format!(
                "{imports} export function run(a:{source_type}, b:{source_type}):{source_type} {{ return a+b; }}"
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
    let compiled = compile(
        "export function run(value:number):boolean { return value < 256; } \
         export function gt(value:number):boolean { return value > -1; } \
         export function sum(value:number):boolean { return value < (128+128); } \
         export function neq(value:number):boolean { return value !== 511; }",
        "func(value:u8)->bool; \
         export gt: func(value:u8)->bool; \
         export sum: func(value:u8)->bool; \
         export neq: func(value:u8)->bool",
    )?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) =
        crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
    for export in ["run", "gt", "sum", "neq"] {
        let run = instance.get_typed_func::<(u8,), (bool,)>(&mut store, export)?;
        for value in [0, 1, 254, 255] {
            assert_eq!(
                run.call(&mut store, (value,))?,
                (true,),
                "{export}: {value}"
            );
        }
    }
    Ok(())
}

#[test]
fn mixed_domains_and_unconstrained_integer_results_widen() -> Result<()> {
    let compiled = compile(
        "function add(a:number,b:number):number { return a+b; } \
         export function run(a:number,b:number):number { return a+b; } \
         export function wide(a:number,b:number):number { return a+b; } \
         export function chained(a:number,b:number):number { return add(a+b,b); } \
         export function big(a:bigint,b:bigint):bigint { return a+b; } \
         export function neg(value:bigint):bigint { return -value; }",
        "func(a:s8,b:u8)->s16; \
         export wide: func(a:u32,b:u32)->f64; \
         export chained: func(a:u32,b:u32)->f64; \
         export big: func(a:s64,b:u64)->u64; \
         export neg: func(value:u64)->s64",
    )?;
    let mut program = Program::from_wasm(compiled.core);
    let unary = [Val::I64(1)];
    for (export, args, expected) in [
        ("run", &[Val::I32(-1), Val::I32(255)][..], Val::I32(254)),
        (
            "wide",
            &[Val::I32(-1), Val::I32(-1)],
            Val::F64(8589934590f64.to_bits()),
        ),
        (
            "chained",
            &[Val::I32(-1), Val::I32(-1)],
            Val::F64(12884901885f64.to_bits()),
        ),
        ("big", &[Val::I64(-1), Val::I64(-1)], Val::I64(-2)),
        ("neg", &unary, Val::I64(-1)),
    ] {
        let actual = program.call(export, args).0[0];
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "{export}");
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
    let source = r#"
        import type { s8, s32, u64 } from "perry:types";
        import type { Byte } from "wit:test:optimization/math";
        function widenAfterAdd(a: s8, b: s8): s32 { const sum: s8 = (a + b) as s8; return sum; }
        export function wrapByte(a: number, b: number): number { const wrapped: Byte = (a + b) as Byte; return wrapped; }
        export function widenS8(a: s8, b: s8): s32 { return widenAfterAdd(a, b); }
        export function big(a: u64, b: u64): u64 { return a + b; }
    "#;
    let compiled = Standard.compile(wit_src, "test", "optimization.ts", source)?;
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
    // Exact parseInt values in aggregates, comparisons, helpers, runtime text/radix and JSON
    // are the `ecma.aggregate.specialization` contract; this checks lexical edge cases,
    // overflow, 64-bit remainder, Math intrinsics, multiplication and alias identity.
    let mut program = Program::from_wasm(
        compile(
            r#"
import type { u64, s64 } from "perry:types";
interface Packet { id: u64; delta: s64 }
type ArithRec = { v: number; opt?: number };
interface Cell { value: number }
interface Node { value: number; next?: Node }
function id(x: u64): u64 { return x; }
function makeArith(x: number): ArithRec { return { v: x * 2, opt: x * 3 }; }
function choose(flag: number): string { return flag > 0 ? "9007199254740993" : "7"; }
function chooseU64Odd(flag: number): string { return flag > 0 ? "18446744073709551615" : "7"; }
function chooseU64Even(flag: number): string { return flag > 0 ? "18446744073709551614" : "6"; }
function relation(a: number, b: number): number {
    return (a === b ? 1 : 0) + (a !== b ? 2 : 0) + (a < b ? 4 : 0) +
        (a <= b ? 8 : 0) + (a > b ? 16 : 0) + (a >= b ? 32 : 0);
}
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
export function parse(flag: number): number {
    const s = choose(flag);
    const a = parseInt("9007199254740993");
    const mul = JSON.stringify(a * 2) === "18014398509481986";
    const overflow = Number.isNaN(parseInt("18446744073709551616"));
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
    const rounded = parseFloat(flag > 0 ? "9007199254740992" : "0.5");
    const minimum = parseInt(flag > 0 ? "-9223372036854775808" : "-7");
    const minimumFloat = parseFloat(flag > 0 ? "-9223372036854775808" : "-7.5");
    const maximum = parseInt(chooseU64Odd(flag));
    const unordered = parseInt(flag > 0 ? "bad" : "1");
    const ordering = relation(exactVal, rounded) === 50 && relation(rounded, exactVal) === 14 &&
        relation(exactVal, exactVal) === 41 && relation(minimum, minimumFloat) === 41 &&
        relation(maximum, 18446744073709551616) === 14 && relation(exactVal, Infinity) === 14 &&
        relation(unordered, exactVal) === 2 && relation(exactVal, unordered) === 2;
    const zero = parseFloat(flag > 0 ? "0" : "1.5");
    const negativeZero = -zero;
    const extrema = Object.is(Math.min(zero, negativeZero), -0) && Object.is(Math.min(negativeZero, zero), -0) &&
        Object.is(Math.max(zero, negativeZero), 0) && Object.is(Math.max(negativeZero, zero), 0) &&
        Math.max(exactVal, rounded) === exactVal && Math.min(rounded, exactVal) === rounded &&
        Number.isNaN(Math.min(unordered, exactVal)) && Number.isNaN(Math.max(exactVal, unordered));
    const magnitude = JSON.stringify(Math.abs(minimum)) === "9223372036854775808" &&
        Object.is(minimum % -1, -0) && Object.is(minimum % 2, -0);
    const aliasSelf: Node = { value: exactVal };
    aliasSelf.next = aliasSelf;
    const sharedCell: Cell = { value: exactVal };
    const sharedPair: Cell[] = [sharedCell].concat([sharedCell]);
    sharedPair[0].value = 1.5;
    const aliasIdentity =
        aliasSelf.next === aliasSelf &&
        (aliasSelf.next?.value ?? 0) === exactVal &&
        sharedPair[1].value === 1.5;
    if (!ordering) return 2;
    if (!extrema) return 3;
    if (!magnitude) return 4;
    return mul && overflow && nonJsWs && numRem && numMath && aliasIdentity ? 1 : 0;
}
"#,
            "func(raw-id:u64,delta:s64,u:u32)->s32; export parse: func(flag:s32)->s32",
        )?
        .core,
    );
    let (res, _) = program.call(
        "run",
        &[
            Val::I64(9007199254740993),
            Val::I64(-9007199254740993),
            Val::I32(3_000_000_000u32 as i32),
        ],
    );
    assert_eq!(res[0].unwrap_i32(), 1);
    assert_eq!(program.call("parse", &[Val::I32(1)]).0[0].unwrap_i32(), 1);

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
    let component = Standard.component(wit_src, "test", "numeric-boundary.ts", boundary_source)?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) = crate::world::instantiate(&engine, &component)?;
    let record = |value| CVal::Record(vec![("value".into(), value)]);
    for (export, expected) in [
        (
            "run-pair",
            CVal::Record(vec![
                ("value".into(), CVal::Float64(42.0)),
                ("name".into(), CVal::String("a".into())),
            ]),
        ),
        ("run-f64", record(CVal::Float64(42.0))),
        ("run-s32", record(CVal::S32(42))),
        ("run-s64", record(CVal::S64(9007199254740993))),
        (
            "run-list",
            CVal::List(vec![CVal::Float64(42.0), CVal::Float64(2.5)]),
        ),
    ] {
        let mut out = [CVal::Bool(false)];
        let function = instance.get_func(&mut store, export).expect(export);
        function.call(&mut store, &[CVal::S32(1)], &mut out)?;
        assert_eq!(out[0], expected, "{export}");
    }
    Ok(())
}
