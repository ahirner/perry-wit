//! WIT integer domains: typed arithmetic, wrap points, widening and exact 64-bit values.
use super::compile;
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
        let mut names = false;
        for payload in Parser::new(0).parse_all(&compiled.core) {
            let payload = payload?;
            if let Payload::CustomSection(section) = &payload {
                names |= section.name() == "name";
            } else if let Some((id, range)) = payload.as_section() {
                stripped.section(&wasm_encoder::RawSection {
                    id,
                    data: &compiled.core[range.start as usize..range.end as usize],
                });
            }
        }
        assert!(names);
        let stripped = stripped.finish();
        assert!(stripped.len() < compiled.core.len());
        let wide = wit.ends_with("64");
        let mut addition = false;
        for bytes in [&compiled.core, &stripped] {
            for payload in Parser::new(0).parse_all(bytes) {
                match payload? {
                    Payload::FunctionSection(section) => assert_eq!(
                        section.count(),
                        if wit.ends_with("8") || wit.ends_with("16") {
                            2
                        } else {
                            1
                        },
                        "{wit}"
                    ),
                    Payload::MemorySection(section) => assert_eq!(section.count(), 0, "{wit}"),
                    Payload::ImportSection(section) => assert_eq!(section.count(), 0, "{wit}"),
                    Payload::ExportSection(section) => {
                        let exports = section
                            .into_iter()
                            .map(|export| export.map(|export| export.name))
                            .collect::<std::result::Result<Vec<_>, _>>()?;
                        assert_eq!(exports, ["run"], "{wit}");
                    }
                    Payload::CodeSectionEntry(body) => {
                        let ops = body
                            .get_operators_reader()?
                            .into_iter()
                            .collect::<std::result::Result<Vec<_>, _>>()?;
                        if wit == "s32" {
                            assert!(
                                matches!(
                                    ops.as_slice(),
                                    [
                                        Operator::LocalGet { local_index: 0 },
                                        Operator::LocalGet { local_index: 1 },
                                        Operator::I32Add,
                                        Operator::End,
                                    ]
                                ),
                                "{ops:?}"
                            );
                        }
                        for op in ops {
                            addition |= matches!(
                                (&op, wide),
                                (Operator::I64Add, true) | (Operator::I32Add, false)
                            );
                            assert!(!format!("{op:?}").contains("F64"), "{wit}: {op:?}");
                            assert!(
                                !matches!(op, Operator::I32Shl | Operator::I32ShrS),
                                "{wit}: use sign-extension instructions"
                            );
                            if matches!(wit, "s32" | "u32" | "s64" | "u64") {
                                assert!(
                                    !matches!(op, Operator::Call { .. }),
                                    "{wit}: exported arithmetic must not forward"
                                );
                            }
                        }
                    }
                    _ => {}
                }
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
    for (wit, body, expected) in [
        ("s8", "a+b", 3),
        ("s16", "a+b", 3),
        ("s8", "a", 1),
        ("s16", "a", 1),
    ] {
        let (parameters, signature) = if body == "a" {
            ("a:number", format!("func(a:{wit})->{wit}"))
        } else {
            ("a:number,b:number", format!("func(a:{wit},b:{wit})->{wit}"))
        };
        let compiled = compile(
            &format!("export function run({parameters}):number {{ return {body}; }}"),
            &signature,
        )?;
        let mut extensions = 0;
        for payload in Parser::new(0).parse_all(&compiled.core) {
            if let Payload::CodeSectionEntry(body) = payload? {
                for op in body.get_operators_reader()? {
                    match op? {
                        Operator::I32Extend8S if wit == "s8" => extensions += 1,
                        Operator::I32Extend16S if wit == "s16" => extensions += 1,
                        Operator::I32Shl | Operator::I32ShrS => panic!("use sign extension"),
                        _ => {}
                    }
                }
            }
        }
        assert_eq!(extensions, expected, "{wit}: {body}");
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
function id(x: u64): u64 { return x; }
export function run(rawId: u64, delta: s64): number {
    const input: Packet = { id: id(rawId), delta };
    const parsed = JSON.parse<Packet>(JSON.stringify(input));
    const ok = JSON.stringify(id(rawId)) === "9007199254740993" && JSON.stringify(parsed.id) === "9007199254740993";
    return ok && parsed.id === rawId && parsed.delta === delta ? 1 : 0;
}
"#,
            "func(raw-id:u64,delta:s64)->s32",
        )?
        .core,
    );
    let (res, _) = roundtrip.call(
        "run",
        &[Val::I64(9007199254740993), Val::I64(-9007199254740993)],
    );
    assert_eq!(res[0].unwrap_i32(), 1);

    // Exact parseInt results flow through records, arrays, optionals, branches and Math.
    let mut parse_int = Program::from_wasm(
        compile(
            r#"
interface Box { raw: string }
interface NumRec { x: number; opt?: number }
interface OuterRec { inner: NumRec; items: number[] }
interface Node { value: number; next?: Node }
function fwd(v: number): number { return v; }
function doubleNode(n: Node): number { return n.value * 2; }
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
    const recOmitted: NumRec = { x: parseInt(s) };
    const rec: NumRec = { x: parseInt(s), opt: flag > 0 ? parseInt(s) : undefined };
    const numRecord =
        JSON.stringify(rec.x + 1) === "9007199254740994" &&
        JSON.stringify(recOmitted) === '{"x":9007199254740993}' &&
        JSON.stringify(rec) === '{"opt":9007199254740993,"x":9007199254740993}';
    const node: Node = { value: parseInt(s), next: { value: parseInt(s) } };
    const recursiveRec =
        JSON.stringify(doubleNode(node)) === "18014398509481986" &&
        node.next !== undefined &&
        JSON.stringify(doubleNode(node.next)) === "18014398509481986";
    const outer: OuterRec = { inner: { x: parseInt(s) }, items: [parseInt(s)] };
    const nestedRecord =
        JSON.stringify(outer.inner.x + 1) === "9007199254740994" &&
        JSON.stringify(outer.items[0] + 1) === "9007199254740994" &&
        JSON.stringify(outer) === '{"inner":{"x":9007199254740993},"items":[9007199254740993]}';
    const recArr: NumRec[] = [{ x: parseInt(s) }];
    const arrayRecord =
        JSON.stringify(recArr[0].x + 1) === "9007199254740994" &&
        JSON.stringify(recArr) === '[{"x":9007199254740993}]';
    const arr = [parseInt(s)];
    const pushed: number[] = [];
    pushed.push(parseInt(s));
    const setArr = [0];
    setArr[0] = parseInt(s);
    const mixedArr = [1, parseInt(s)];
    const concatMixed = arr.concat([1.5]);
    const concatSame = arr.concat(arr);
    const spreadMixed = [1.5, ...arr, 2.5];
    const extended: number[] = [1.5];
    extended.push(...arr);
    const numArray =
        JSON.stringify(arr[0] + 1) === "9007199254740994" &&
        JSON.stringify(arr) === "[9007199254740993]" &&
        JSON.stringify(pushed) === "[9007199254740993]" &&
        JSON.stringify(setArr) === "[9007199254740993]" &&
        JSON.stringify(mixedArr) === "[1,9007199254740993]" &&
        JSON.stringify(concatMixed) === "[9007199254740993,1.5]" &&
        JSON.stringify(concatSame) === "[9007199254740993,9007199254740993]" &&
        JSON.stringify(spreadMixed) === "[1.5,9007199254740993,2.5]" &&
        JSON.stringify(extended) === "[1.5,9007199254740993]";
    const many: NumRec[] = [
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) },
        { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }, { x: parseInt(s) }
    ];
    const summarizedLayout = JSON.stringify(many[flag - 1].x * 2) === "18014398509481986";
    const opt: number | undefined = flag > 0 ? parseInt(s) : undefined;
    const numOptional =
        JSON.stringify(opt === undefined ? 0 : opt + 1) === "9007199254740994" &&
        JSON.stringify(opt) === "9007199254740993";
    const br = flag > 0 ? parseInt(s) : 1;
    const numBranch = JSON.stringify(br + 1) === "9007199254740994";
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
    return eq && mul && cmp && helper && overflow && radixWrap && runtimeText && runtimeRadix && runtimeJson && nonJsWs && numRecord && recursiveRec && nestedRecord && arrayRecord && numArray && summarizedLayout && numOptional && numBranch && numRem && numMath ? 1 : 0;
}
"#,
            "func(flag:s32)->s32",
        )?
        .core,
    );
    assert_eq!(parse_int.call("run", &[Val::I32(1)]).0[0].unwrap_i32(), 1);
    Ok(())
}
