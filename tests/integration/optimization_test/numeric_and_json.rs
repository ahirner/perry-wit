use super::compile;
use anyhow::Result;

#[test]
fn perry_types_and_wit_sdk_integer_aliases_enforce_wrapping_and_width_widening() -> Result<()> {
    let wit_src = r#"
        package test:optimization;
        interface math {
            type byte = u8;
        }
        world test {
            import math;
            export wrap-local: func(a: u32, b: u32) -> u32;
            export widen-helper: func(a: u32, b: u32) -> u32;
            export narrow-helper: func(a: u32, b: u32) -> u32;
            export signed-step: func(a: s32, b: s32) -> s32;
            export wit-alias: func(a: u32, b: u32) -> u32;
            export user-spelling: func(a: f64, b: f64) -> f64;
        }
    "#;
    let (resolve, package) =
        perry_wit::component::wit::resolve_source("optimization.wit", wit_src)?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let source = r#"
        import type { u8, s8, u32 } from "perry:types";
        import type { Byte } from "wit:test:optimization/math";

        type s32 = number;

        function addWiden(x: u8, y: u8): u32 {
            return x + y;
        }

        function addNarrow(x: u8, y: u8): u8 {
            return (x + y) as u8;
        }

        export function wrapLocal(a: number, b: number): number {
            const wrapped: u8 = (a + b) as u8;
            return wrapped;
        }

        export function widenHelper(a: number, b: number): number {
            return addWiden(a as u8, b as u8);
        }

        export function narrowHelper(a: number, b: number): number {
            return addNarrow(a as u8, b as u8);
        }

        export function signedStep(a: number, b: number): number {
            let acc: s8 = a as s8;
            acc = (acc + (b as s8)) as s8;
            return acc;
        }

        export function witAlias(a: number, b: number): number {
            const wrapped: Byte = (a + b) as Byte;
            return wrapped;
        }

        export function userSpelling(a: s32, b: s32): s32 {
            return a + b;
        }
    "#;
    let compiled =
        perry_wit::compile_for_world(source, "optimization.ts", &resolve, world, true)?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program
            .call("wrap-local", &[wasmtime::Val::I32(255), wasmtime::Val::I32(255)])
            .0[0]
            .unwrap_i32(),
        254
    );
    assert_eq!(
        program
            .call("widen-helper", &[wasmtime::Val::I32(255), wasmtime::Val::I32(255)])
            .0[0]
            .unwrap_i32(),
        510
    );
    assert_eq!(
        program
            .call("narrow-helper", &[wasmtime::Val::I32(255), wasmtime::Val::I32(255)])
            .0[0]
            .unwrap_i32(),
        254
    );
    assert_eq!(
        program
            .call("signed-step", &[wasmtime::Val::I32(120), wasmtime::Val::I32(20)])
            .0[0]
            .unwrap_i32(),
        -116
    );
    assert_eq!(
        program
            .call("wit-alias", &[wasmtime::Val::I32(255), wasmtime::Val::I32(2)])
            .0[0]
            .unwrap_i32(),
        1
    );
    let user_res = program
        .call(
            "user-spelling",
            &[
                wasmtime::Val::F64(1.5f64.to_bits()),
                wasmtime::Val::F64(2.25f64.to_bits()),
            ],
        )
        .0[0]
        .unwrap_f64();
    assert_eq!(user_res, 3.75);
    Ok(())
}

#[test]
fn builtin_numeric_contracts_and_string_number_parsers_match_js_semantics() -> Result<()> {
    let wit_src = r#"
        package test:optimization;
        world test {
            export parse-int-cases: func() -> s32;
            export parse-float-cases: func() -> f64;
            export number-coercion-cases: func() -> f64;
            export predicates-and-constants: func() -> s32;
            export math-integer-contracts: func(a: s32, b: s32) -> s32;
        }
    "#;
    let (resolve, package) =
        perry_wit::component::wit::resolve_source("optimization.wit", wit_src)?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let source = r#"
        import type { s32, u8 } from "perry:types";

        export function parseIntCases(): number {
            const a = parseInt("  -42px");
            const b = parseInt("0x10");
            const c = parseInt("101", 2);
            const d = Number.parseInt("ff", 16);
            const bad = parseInt("10", 37);
            const empty = parseInt("   ");
            if (!Number.isNaN(bad) || !Number.isNaN(empty)) {
                return -999;
            }
            return a + b + c + d;
        }

        export function parseFloatCases(): number {
            const a = parseFloat("  3.5e1abc");
            const b = Number.parseFloat("-0.25");
            const inf = parseFloat("Infinity");
            const empty = parseFloat("   ");
            if (inf !== Number.POSITIVE_INFINITY || !Number.isNaN(empty)) {
                return -1;
            }
            return a + b;
        }

        export function numberCoercionCases(): number {
            const a = Number("  0x20  ");
            const b = +"0b101";
            const c = Number("0o10");
            const d = +"   ";
            const bad = Number("12px");
            if (!Number.isNaN(bad)) {
                return -1;
            }
            return a + b + c + d;
        }

        export function predicatesAndConstants(): number {
            let score: s32 = 0 as s32;
            if (Number.isInteger(42) && !Number.isInteger(42.5) && !Number.isInteger(NaN)) {
                score = (score + 1) as s32;
            }
            if (
                Number.isSafeInteger(Number.MAX_SAFE_INTEGER) &&
                Number.isSafeInteger(Number.MIN_SAFE_INTEGER) &&
                !Number.isSafeInteger(Number.MAX_SAFE_INTEGER + 1)
            ) {
                score = (score + 2) as s32;
            }
            if (
                Number.isFinite(123.45) &&
                !Number.isFinite(Infinity) &&
                isFinite(0) &&
                !isFinite(NaN)
            ) {
                score = (score + 4) as s32;
            }
            if (Number.isNaN(NaN) && isNaN(NaN) && !Number.isNaN(0) && Number.EPSILON > 0) {
                score = (score + 8) as s32;
            }
            return score;
        }

        export function mathIntegerContracts(a: number, b: number): number {
            const absA = Math.abs(a);
            const hi = Math.max(absA, b);
            const lo = Math.min(absA, b);
            const rounded = Math.floor(hi) + Math.ceil(lo) + Math.trunc(1.0) + Math.round(2.0);
            const code: u8 = ("A".codePointAt(0)!) as u8;
            return rounded + code;
        }
    "#;
    let compiled =
        perry_wit::compile_for_world(source, "optimization.ts", &resolve, world, true)?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program.call("parse-int-cases", &[]).0[0].unwrap_i32(),
        -42 + 16 + 5 + 255
    );
    assert_eq!(
        program.call("parse-float-cases", &[]).0[0].unwrap_f64(),
        34.75
    );
    assert_eq!(
        program.call("number-coercion-cases", &[]).0[0].unwrap_f64(),
        32.0 + 5.0 + 8.0 + 0.0
    );
    assert_eq!(
        program.call("predicates-and-constants", &[]).0[0].unwrap_i32(),
        15
    );
    assert_eq!(
        program
            .call(
                "math-integer-contracts",
                &[wasmtime::Val::I32(-10), wasmtime::Val::I32(4)],
            )
            .0[0]
            .unwrap_i32(),
        10 + 4 + 1 + 2 + 65
    );
    Ok(())
}

#[test]
fn exact_64_bit_json_integer_codecs_preserve_precision() -> Result<()> {
    let wit_src = r#"
        package test:optimization;
        world test {
            record counters {
                signed: s64,
                unsigned: u64,
            }
            export roundtrip-signed: func(value: s64) -> s64;
            export roundtrip-unsigned: func(value: u64) -> u64;
            export roundtrip-record: func(value: counters) -> counters;
            export roundtrip-record-check: func(signed: s64, unsigned: u64) -> bool;
        }
    "#;
    let (resolve, package) =
        perry_wit::component::wit::resolve_source("optimization.wit", wit_src)?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let source = r#"
        import type { s64, u64 } from "perry:types";

        interface Counters {
            signed: s64;
            unsigned: u64;
        }

        export function roundtripSigned(value: s64): s64 {
            return JSON.parse<s64>(JSON.stringify(value));
        }

        export function roundtripUnsigned(value: u64): u64 {
            return JSON.parse<u64>(JSON.stringify(value));
        }

        export function roundtripRecord(value: Counters): Counters {
            return JSON.parse<Counters>(JSON.stringify(value));
        }

        export function roundtripRecordCheck(signed: s64, unsigned: u64): boolean {
            const input: Counters = { signed, unsigned };
            const output = JSON.parse<Counters>(JSON.stringify(input));
            return output.signed === signed && output.unsigned === unsigned;
        }
    "#;
    let compiled =
        perry_wit::compile_for_world(source, "optimization.ts", &resolve, world, true)?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    for signed in [0i64, 1, -1, 9007199254740993, -9007199254740993, i64::MAX, i64::MIN] {
        assert_eq!(
            program
                .call("roundtrip-signed", &[wasmtime::Val::I64(signed)])
                .0[0]
                .unwrap_i64(),
            signed
        );
    }
    for unsigned in [0u64, 1, 9007199254740993, (i64::MAX as u64) + 42, u64::MAX] {
        assert_eq!(
            program
                .call(
                    "roundtrip-unsigned",
                    &[wasmtime::Val::I64(unsigned as i64)],
                )
                .0[0]
                .unwrap_i64() as u64,
            unsigned
        );
        assert_eq!(
            program
                .call(
                    "roundtrip-record-check",
                    &[
                        wasmtime::Val::I64(-9007199254740993),
                        wasmtime::Val::I64(unsigned as i64),
                    ],
                )
                .0[0]
                .unwrap_i32(),
            1
        );
    }
    Ok(())
}

#[test]
fn aggregate_and_optional_integer_facts_propagate_through_records_and_arrays() -> Result<()> {
    let source = r#"
        import type { u8, u32 } from "perry:types";

        interface Boxed {
            head: u8;
            tail?: u32;
        }

        export function run(a: number, b: number): number {
            const box: Boxed = {
                head: (a as u8),
                tail: (b as u32),
            };
            const tail: u32 = box.tail !== undefined ? box.tail : (0 as u32);
            const list: u32[] = [box.head as u32, tail];
            return list[0]! + list[1]!;
        }
    "#;
    let compiled = compile(source, "func(a: u8, b: u32) -> u32")?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program
            .call("run", &[wasmtime::Val::I32(250), wasmtime::Val::I32(1000)])
            .0[0]
            .unwrap_i32(),
        1250
    );
    Ok(())
}

#[test]
fn bounded_context_specialization_and_range_widening_remain_sound() -> Result<()> {
    let source = r#"
        import type { u8, s8, u16, s16, u32, s32 } from "perry:types";

        function addPair(a: number, b: number): s32 {
            return (a + b) as s32;
        }

        export function run(x: number): number {
            const v_u8: u8 = x as u8;
            const v_s8: s8 = x as s8;
            const v_u16: u16 = x as u16;
            const v_s16: s16 = x as s16;
            const v_u32: u32 = x as u32;
            const v_s32: s32 = x as s32;

            let total: s32 = 0 as s32;
            // 6 x 6 = 36 distinct (arg0, arg1) integer domain pairs (> 32 precise contexts)
            total = (total + addPair(v_u8, v_u8)) as s32;
            total = (total + addPair(v_u8, v_s8)) as s32;
            total = (total + addPair(v_u8, v_u16)) as s32;
            total = (total + addPair(v_u8, v_s16)) as s32;
            total = (total + addPair(v_u8, v_u32)) as s32;
            total = (total + addPair(v_u8, v_s32)) as s32;

            total = (total + addPair(v_s8, v_u8)) as s32;
            total = (total + addPair(v_s8, v_s8)) as s32;
            total = (total + addPair(v_s8, v_u16)) as s32;
            total = (total + addPair(v_s8, v_s16)) as s32;
            total = (total + addPair(v_s8, v_u32)) as s32;
            total = (total + addPair(v_s8, v_s32)) as s32;

            total = (total + addPair(v_u16, v_u8)) as s32;
            total = (total + addPair(v_u16, v_s8)) as s32;
            total = (total + addPair(v_u16, v_u16)) as s32;
            total = (total + addPair(v_u16, v_s16)) as s32;
            total = (total + addPair(v_u16, v_u32)) as s32;
            total = (total + addPair(v_u16, v_s32)) as s32;

            total = (total + addPair(v_s16, v_u8)) as s32;
            total = (total + addPair(v_s16, v_s8)) as s32;
            total = (total + addPair(v_s16, v_u16)) as s32;
            total = (total + addPair(v_s16, v_s16)) as s32;
            total = (total + addPair(v_s16, v_u32)) as s32;
            total = (total + addPair(v_s16, v_s32)) as s32;

            total = (total + addPair(v_u32, v_u8)) as s32;
            total = (total + addPair(v_u32, v_s8)) as s32;
            total = (total + addPair(v_u32, v_u16)) as s32;
            total = (total + addPair(v_u32, v_s16)) as s32;
            total = (total + addPair(v_u32, v_u32)) as s32;
            total = (total + addPair(v_u32, v_s32)) as s32;

            total = (total + addPair(v_s32, v_u8)) as s32;
            total = (total + addPair(v_s32, v_s8)) as s32;
            total = (total + addPair(v_s32, v_u16)) as s32;
            total = (total + addPair(v_s32, v_s16)) as s32;
            total = (total + addPair(v_s32, v_u32)) as s32;
            total = (total + addPair(v_s32, v_s32)) as s32;

            // Also exercise loop range widening
            let i: s32 = 0 as s32;
            while (i < (3 as s32)) {
                total = (total + i) as s32;
                i = (i + (1 as s32)) as s32;
            }
            return total;
        }
    "#;
    let compiled = compile(source, "func(x: s32) -> s32")?;
    assert!(
        compiled.allocation_report.contains("summarized_contexts=5"),
        "expected 5 summarized contexts in report: {}",
        compiled.allocation_report
    );
    assert!(
        !compiled.allocation_report.contains("range_widenings=0"),
        "expected non-zero range widenings in report: {}",
        compiled.allocation_report
    );
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    // 36 pairs * (10 + 10) + (0 + 1 + 2) = 723
    assert_eq!(
        program.call("run", &[wasmtime::Val::I32(10)]).0[0].unwrap_i32(),
        723
    );
    Ok(())
}

#[test]
fn normalize_json_copy_rewrites_parse_stringify_and_preserves_edge_cases() -> Result<()> {
    let wit_src = r#"
        package test:optimization;
        world test {
            export copy-isolation: func(id: u64, delta: s64) -> s32;
            export edge-normalization: func() -> s32;
            export non-finite-throws: func() -> s32;
            export untyped-missed: func() -> s32;
        }
    "#;
    let (resolve, package) =
        perry_wit::component::wit::resolve_source("optimization.wit", wit_src)?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let source = r#"
        import type { s32, s64, u64 } from "perry:types";

        interface Inner {
            count: s32;
            items: number[];
        }

        interface FullItem {
            id: u64;
            delta: s64;
            inner: Inner;
            opt?: number;
            extra: string;
        }

        interface ProjectedItem {
            id: u64;
            delta: s64;
            inner: Inner;
            opt?: number;
            missingOpt?: string;
        }

        interface NullableNums {
            nanVal: number | null;
            infVal: number | null;
            negZero: number;
            arr: (number | null)[];
        }

        interface SourceNums {
            nanVal: number;
            infVal: number;
            negZero: number;
            arr: (number | undefined)[];
        }

        type Tagged = { tag: "num"; val: number } | { tag: "empty" };

        export function copyIsolation(id: u64, delta: s64): number {
            const orig: FullItem = {
                id,
                delta,
                inner: { count: 10 as s32, items: [1, 2, 3] },
                extra: "dropped",
            };
            const copy = JSON.parse<ProjectedItem>(JSON.stringify(orig));
            copy.inner.count = 99 as s32;
            copy.inner.items[0] = 100;

            const vOrig: Tagged = { tag: "num", val: -0.0 };
            const vCopy = JSON.parse<Tagged>(JSON.stringify(vOrig));
            let variantCheck = 0;
            if (vCopy.tag === "num" && 1 / vCopy.val === Number.POSITIVE_INFINITY) {
                variantCheck = 1;
            }

            const tupleOrig: [number, string] = [-0.0, "ok"];
            const tupleCopy = JSON.parse<[number, string]>(JSON.stringify(tupleOrig));
            const tupleCheck =
                1 / tupleCopy[0] === Number.POSITIVE_INFINITY && tupleCopy[1] === "ok" ? 1 : 0;

            if (
                orig.inner.count === (10 as s32) &&
                orig.inner.items[0] === 1 &&
                copy.id === id &&
                copy.delta === delta &&
                copy.opt === undefined &&
                copy.missingOpt === undefined &&
                variantCheck === 1 &&
                tupleCheck === 1
            ) {
                return 1;
            }
            return 0;
        }

        export function edgeNormalization(): number {
            const src: SourceNums = {
                nanVal: Number.NaN,
                infVal: Number.POSITIVE_INFINITY,
                negZero: -0.0,
                arr: [1.5, undefined, -0.0],
            };
            const dst = JSON.parse<NullableNums>(JSON.stringify(src));
            if (
                dst.nanVal === null &&
                dst.infVal === null &&
                1 / dst.negZero === Number.POSITIVE_INFINITY &&
                dst.arr.length === 3 &&
                dst.arr[0] === 1.5 &&
                dst.arr[1] === null &&
                dst.arr[2] !== null &&
                1 / dst.arr[2]! === Number.POSITIVE_INFINITY
            ) {
                return 1;
            }
            return 0;
        }

        export function nonFiniteThrows(): number {
            let caught = 0;
            try {
                const bad: { x: number } = { x: Number.NaN };
                const _res = JSON.parse<{ x: number }>(JSON.stringify(bad));
            } catch (_e) {
                caught += 1;
            }
            try {
                const badInf: { x: number } = { x: Number.NEGATIVE_INFINITY };
                const _res = JSON.parse<{ x: number }>(JSON.stringify(badInf));
            } catch (_e) {
                caught += 10;
            }
            return caught;
        }

        export function untypedMissed(): number {
            const raw = JSON.parse(JSON.stringify({ a: 7 }));
            const parsed = JSON.parse<Inner>('{"count":10,"items":[1],"unknownResidue":77}');
            const roundtripped = JSON.parse<Inner>(JSON.stringify(parsed));
            const out = JSON.stringify(roundtripped);
            return raw !== null && out === '{"count":10,"items":[1],"unknownResidue":77}' ? 1 : 0;
        }
    "#;
    let compiled =
        perry_wit::compile_for_world(source, "optimization.ts", &resolve, world, true)?;
    assert!(
        compiled
            .allocation_report
            .contains("parse_stringify_rewrites=6"),
        "expected 6 parse_stringify_rewrites in report: {}",
        compiled.allocation_report
    );
    assert!(
        compiled
            .allocation_report
            .contains("parse_stringify_missed=2"),
        "expected 2 parse_stringify_missed in report: {}",
        compiled.allocation_report
    );
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    let exact_u64 = 9_007_199_254_740_993_u64 as i64;
    let exact_s64 = -9_007_199_254_740_993_i64;
    assert_eq!(
        program
            .call(
                "copy-isolation",
                &[
                    wasmtime::Val::I64(exact_u64),
                    wasmtime::Val::I64(exact_s64),
                ],
            )
            .0[0]
            .unwrap_i32(),
        1
    );
    assert_eq!(
        program.call("edge-normalization", &[]).0[0].unwrap_i32(),
        1
    );
    assert_eq!(
        program.call("non-finite-throws", &[]).0[0].unwrap_i32(),
        11
    );
    assert_eq!(
        program.call("untyped-missed", &[]).0[0].unwrap_i32(),
        1
    );
    Ok(())
}
