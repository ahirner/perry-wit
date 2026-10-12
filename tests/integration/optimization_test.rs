//! Binary structure and behavior of optimized WIT-constrained programs.
use anyhow::Result;
use wasmparser::{Parser, Payload};

fn mir_program(
    signature: &str,
    arguments: &str,
    body: &str,
) -> Result<crate::core_program::Program> {
    use pliron::{
        builtin::{
            op_interfaces::SingleBlockRegionInterface,
            ops::{FuncOp, ModuleOp},
        },
        context::Context,
        irfmt::parsers::spaced,
        linked_list::ContainsLinkedList,
        operation::Operation,
        parsable::parse_from_str,
        printable::Printable,
    };
    let ctx = &mut Context::new();
    let text = format!(
        "builtin.module @numeric {{ ^module(): builtin.func @run: builtin.function <{signature}> {{ ^entry({arguments}): {body} }} }}"
    );
    let root = parse_from_str(spaced(Operation::top_level_parser()), ctx, &text)
        .map_err(|error| anyhow::anyhow!("{}", error.disp(ctx)))?;
    let module = Operation::get_op::<ModuleOp>(root, ctx).unwrap();
    let operations: Vec<_> = module.get_body(ctx, 0).deref(ctx).iter(ctx).collect();
    for op in operations {
        if let Some(function) = Operation::get_op::<FuncOp>(op, ctx) {
            perry_mir::calls::set_exported(ctx, &function);
        }
    }
    let mut analyses = perry_mir::pipeline::canonicalize(root, ctx)
        .map_err(|error| anyhow::anyhow!("{}", error.disp(ctx)))?;
    let bytes = perry_codegen::compile(ctx, &module, &perry_codegen::Exports::Raw, &mut analyses)?;
    Ok(crate::core_program::Program::from_wasm(bytes))
}

fn compile(source: &str, signature: &str) -> Result<perry_wit::Compiled> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "optimization.wit",
        &format!("package test:optimization; world test {{ export run: {signature}; }}"),
    )?;
    let world = resolve.select_world(&[package], Some("test"))?;
    perry_wit::compile_for_world(source, "optimization.ts", &resolve, world, true)
}

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
        let wide = wit.ends_with("64");
        let mut addition = false;
        for payload in Parser::new(0).parse_all(&compiled.core) {
            match payload? {
                Payload::FunctionSection(section) => assert_eq!(section.count(), 2, "{wit}"),
                Payload::MemorySection(section) => assert_eq!(section.count(), 0, "{wit}"),
                Payload::ImportSection(section) => assert_eq!(section.count(), 0, "{wit}"),
                Payload::ExportSection(section) => {
                    let exports: Vec<_> = section
                        .into_iter()
                        .map(|export| export.unwrap().name.to_owned())
                        .collect();
                    assert_eq!(exports, ["run"], "{wit}");
                }
                Payload::CodeSectionEntry(body) => {
                    for op in body.get_operators_reader()? {
                        let op = op?;
                        addition |= matches!(
                            (&op, wide),
                            (wasmparser::Operator::I64Add, true)
                                | (wasmparser::Operator::I32Add, false)
                        );
                        assert!(!format!("{op:?}").contains("F64"), "{wit}: {op:?}");
                    }
                }
                _ => {}
            }
        }
        assert!(addition, "{wit}");
        assert!(compiled.core.len() < 2963, "{wit}");
        let mut program = crate::core_program::Program::from_wasm(compiled.core);
        let args = if wide {
            vec![wasmtime::Val::I64(a), wasmtime::Val::I64(b)]
        } else {
            vec![wasmtime::Val::I32(a as i32), wasmtime::Val::I32(b as i32)]
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

#[tokio::test(flavor = "current_thread")]
async fn integer_domains_survive_async_frames_and_result_slots() -> Result<()> {
    use wasmtime::{
        Store,
        component::{Component, Linker, Val},
    };
    for (domain, source_type, a, b, expected) in [
        ("u8", "number", Val::U8(255), Val::U8(2), Val::U8(1)),
        (
            "s32",
            "number",
            Val::S32(i32::MAX),
            Val::S32(2),
            Val::S32(i32::MIN + 1),
        ),
        (
            "u64",
            "bigint",
            Val::U64(u64::MAX),
            Val::U64(2),
            Val::U64(1),
        ),
        (
            "s64",
            "bigint",
            Val::S64(9_007_199_254_740_993),
            Val::S64(2),
            Val::S64(9_007_199_254_740_995),
        ),
    ] {
        for source in [
            format!(
                "function add(a:{source_type},b:{source_type}):{source_type} {{ return a+b; }} export async function run(a:{source_type},b:{source_type}):Promise<{source_type}> {{ await Promise.resolve(1); return add(a,b); }}"
            ),
            format!(
                "async function add(a:{source_type},b:{source_type}):Promise<{source_type}> {{ await Promise.resolve(1); return a+b; }} export async function run(a:{source_type},b:{source_type}):Promise<{source_type}> {{ return await add(a,b); }}"
            ),
            format!(
                "export async function run(a:{source_type},b:{source_type}):Promise<{source_type}> {{ return await Promise.resolve(a+b); }}"
            ),
        ] {
            let compiled = compile(
                &source,
                &format!("async func(a:{domain},b:{domain})->{domain}"),
            )?;
            let engine = crate::engine::async_engine()?;
            let component = Component::new(&engine, &compiled.component.unwrap().stripped)?;
            let mut store = Store::new(&engine, ());
            let instance = Linker::new(&engine)
                .instantiate_async(&mut store, &component)
                .await?;
            let run = instance.get_func(&mut store, "run").unwrap();
            let mut results = [Val::Bool(false)];
            run.call_async(&mut store, &[a.clone(), b.clone()], &mut results)
                .await?;
            assert_eq!(results[0], expected, "{domain}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn promise_joins_and_escaping_payloads_converge_on_compatible_representations() -> Result<()>
{
    use wasmtime::{
        Store,
        component::{Component, Linker, Val},
    };
    let helper = "async function add(a:number,b:number):Promise<number> { await Promise.resolve(1); return a+b; } async function sum(n:number):Promise<number> { if(n<=0)return 0; return n+await sum(n-1); }";
    for (body, result_type, yes, no) in [
        (
            "const pending = flag ? add(a,b) : Promise.resolve(a+b); return await pending;",
            "u8",
            Val::U8(1),
            Val::U8(1),
        ),
        (
            "const pending = flag ? add(a,b) : Promise.resolve(0.5); return await pending;",
            "f64",
            Val::Float64(257.0),
            Val::Float64(0.5),
        ),
        (
            "const pending = flag ? add(a,b) : add(0.5,b); return await pending;",
            "f64",
            Val::Float64(257.0),
            Val::Float64(2.5),
        ),
        (
            "const values = await Promise.all([add(a,b), Promise.resolve(0.5)]); return values[0] + values[1];",
            "f64",
            Val::Float64(257.5),
            Val::Float64(257.5),
        ),
        (
            "let pending = Promise.resolve(a); for(let i=0;i<b;i++){ pending=add(await pending,a); } return await pending;",
            "u8",
            Val::U8(253),
            Val::U8(253),
        ),
        ("return await sum(b);", "u32", Val::U32(3), Val::U32(3)),
    ] {
        let source = format!(
            "{helper} export async function run(a:number,b:number,flag:boolean):Promise<number> {{ {body} }}"
        );
        let compiled = compile(
            &source,
            &format!("async func(a:u8,b:u8,flag:bool)->{result_type}"),
        )
        .map_err(|error| anyhow::anyhow!("{body}: {error:#}"))?;
        let engine = crate::engine::async_engine()?;
        let component = Component::new(&engine, &compiled.component.unwrap().stripped)?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let run = instance.get_func(&mut store, "run").unwrap();
        for (flag, expected) in [(true, yes), (false, no)] {
            let mut results = [Val::Bool(false)];
            run.call_async(
                &mut store,
                &[Val::U8(255), Val::U8(2), Val::Bool(flag)],
                &mut results,
            )
            .await?;
            assert_eq!(results[0], expected, "{body}; flag={flag}");
        }
    }
    Ok(())
}

#[test]
fn numeric_constraints_cross_calls_and_loop_carried_values() -> Result<()> {
    for source in [
        "export function run(a:number,b:number):number { return a+b; }",
        "function add(a:number,b:number):number { return a+b; } export function run(a:number,b:number):number { return add(a,b); }",
        "function identity(a:number):number { return a; } export function run(a:number,b:number):number { return identity(a+b); }",
        "export function run(a:number,b:number):number { let value=a; for(let i=0;i<b;i++) value+=1; return value; }",
    ] {
        let compiled = compile(source, "func(a:u8,b:u8)->u32")
            .map_err(|error| anyhow::anyhow!("{source}\n{error:#}"))?;
        let engine = wasmtime::Engine::default();
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(u8, u8), (u32,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (255, 255))?, (510,), "{source}");
    }
    let compiled = compile(
        "export function run(n:number):number { if(n<=0)return 0; return n+run(n-1); }",
        "func(n:s32)->s32",
    )?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program.call("run", &[wasmtime::Val::I32(20)]).0[0].unwrap_i32(),
        210
    );
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
fn fractional_operations_and_unconstrained_callers_keep_number_semantics() -> Result<()> {
    let compiled = compile(
        "export function run(a:number,b:number):number { return a/b + a%b; }",
        "func(a:s32,b:s32)->f64",
    )?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    for (a, b) in [(5, 2), (-5, 2), (7, -3)] {
        assert_eq!(
            program
                .call("run", &[wasmtime::Val::I32(a), wasmtime::Val::I32(b)])
                .0[0]
                .unwrap_f64(),
            a as f64 / b as f64 + a as f64 % b as f64
        );
    }
    let source = include_str!("../fixtures/optimization/shared.ts");
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("shared.wit", "package test:shared; world test { export narrow:func(x:u8,y:u8)->u8; export wide:func(x:u8,y:u8)->u32; export floating:func(x:f64,y:f64)->f64; }")?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let compiled = perry_wit::compile_for_world(source, "shared.ts", &resolve, world, true)?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) =
        crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
    assert_eq!(
        instance
            .get_typed_func::<(u8, u8), (u8,)>(&mut store, "narrow")?
            .call(&mut store, (255, 255))?,
        (254,)
    );
    assert_eq!(
        instance
            .get_typed_func::<(u8, u8), (u32,)>(&mut store, "wide")?
            .call(&mut store, (255, 255))?,
        (510,)
    );
    assert_eq!(
        instance
            .get_typed_func::<(f64, f64), (f64,)>(&mut store, "floating")?
            .call(&mut store, (0.25, 0.5))?,
        (0.75,)
    );
    assert_eq!(
        crate::core_program::Program::new("export function run(x:number):number { return x+0.5; }")
            .number("run", 0.25),
        0.75
    );
    Ok(())
}

#[test]
fn mixed_domains_and_unconstrained_integer_results_widen() -> Result<()> {
    let compiled = compile(
        "export function run(a:number,b:number):number { return a+b; }",
        "func(a:s8,b:u8)->s16",
    )?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program
            .call("run", &[wasmtime::Val::I32(-1), wasmtime::Val::I32(255)])
            .0[0]
            .unwrap_i32(),
        254
    );
    let compiled = compile(
        "export function run(a:number,b:number):number { return a+b; }",
        "func(a:u32,b:u32)->f64",
    )?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program
            .call("run", &[wasmtime::Val::I32(-1), wasmtime::Val::I32(-1)])
            .0[0]
            .unwrap_f64(),
        8589934590.0
    );
    let compiled = compile(
        "export function run(a:bigint,b:bigint):bigint { return a+b; }",
        "func(a:s64,b:u64)->u64",
    )?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program
            .call("run", &[wasmtime::Val::I64(-1), wasmtime::Val::I64(-1)])
            .0[0]
            .unwrap_i64(),
        -2
    );
    let compiled = compile(
        "export function run(value:bigint):bigint { return -value; }",
        "func(value:u64)->u64",
    )?;
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program.call("run", &[wasmtime::Val::I64(1)]).0[0].unwrap_i64(),
        -1
    );
    Ok(())
}

#[test]
fn range_queries_follow_actual_wrap_points() -> Result<()> {
    let mut raw = crate::core_program::Program::new(
        "function negative(value:number):number { return 0-value; } export function run():number { const values=[10]; const index=negative(18446744073709549568); if(index<values.length){const item=values[index];return item===undefined?7:item;} return 0; }",
    );
    assert_eq!(raw.call("run", &[]).0[0].unwrap_f64(), 7.0);
    let compiled = compile(
        "export function run(flag:boolean):number { const values=[10]; const index=200; if(flag && index<values.length){const item=values[index];return item===undefined?7:item;} return index; }",
        "func(flag:bool)->s8",
    )?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) =
        crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
    let run = instance.get_typed_func::<(bool,), (i8,)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, (true,))?, (7,));
    assert_eq!(run.call(&mut store, (false,))?, (-56,));
    Ok(())
}

#[test]
fn independent_scalar_boundaries_have_no_runtime_roots() -> Result<()> {
    for source in [
        "export function run(a:number,b:number):number { return a+b; }",
        "export function run(a:number,b:number):number { let sum=b; for(let i=0;i<a;i++)sum+=1; return sum; }",
        "export function run(a:number,b:number):number { return a+b; } export function unused():string { return String(Math.random()); }",
    ] {
        let compiled = compile(source, "func(a:f64,b:f64)->f64")?;
        let mut functions = 0;
        let mut exports = Vec::new();
        for payload in Parser::new(0).parse_all(&compiled.core) {
            match payload? {
                Payload::FunctionSection(section) => functions = section.count(),
                Payload::MemorySection(section) => assert_eq!(section.count(), 0),
                Payload::ImportSection(section) => assert_eq!(section.count(), 0),
                Payload::ExportSection(section) => {
                    exports = section
                        .into_iter()
                        .map(|e| e.map(|e| e.name.to_owned()))
                        .collect::<std::result::Result<_, _>>()?;
                }
                _ => {}
            }
        }
        assert_eq!(functions, 2, "implementation and wrapper only");
        assert_eq!(exports, ["run"]);
        assert!(compiled.core.len() < 2963);
        let engine = wasmtime::Engine::default();
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(f64, f64), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (5.0, 2.0))?, (7.0,));
    }
    Ok(())
}

#[test]
fn scalar_signatures_do_not_hide_runtime_effects() -> Result<()> {
    for source in [
        "export function run(a:number):number { return String(a).length; }",
        "function check(a:number):number { if(a<0)throw new Error('negative'); return a; } export function run(a:number):number { return check(a); }",
        "let count=0; export function run(a:number):number { count+=a; return count; }",
    ] {
        let compiled = compile(source, "func(a:f64)->f64")?;
        let mut exports = Vec::new();
        for payload in Parser::new(0).parse_all(&compiled.core) {
            if let Payload::ExportSection(section) = payload? {
                exports = section
                    .into_iter()
                    .map(|e| e.map(|e| e.name.to_owned()))
                    .collect::<std::result::Result<_, _>>()?;
            }
        }
        assert!(exports.iter().any(|e| e == "memory"), "{source}");
        assert!(exports.iter().any(|e| e == "cabi_post_run"), "{source}");
        assert!(!exports.iter().any(|e| e == "cabi_realloc"), "{source}");
        let engine = wasmtime::Engine::default();
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (1.0,))?, (1.0,));
    }
    Ok(())
}

#[test]
fn unused_host_imports_do_not_root_the_allocator() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("unused.wit", "package test:unused; world test { import label: func() -> string; export run: func(a:f64)->f64; }")?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let compiled = perry_wit::compile_for_world(
        "export function run(a:number):number { return String(a).length; }",
        "unused.ts",
        &resolve,
        world,
        true,
    )?;
    for payload in Parser::new(0).parse_all(&compiled.core) {
        if let Payload::ExportSection(exports) = payload? {
            assert!(
                exports
                    .into_iter()
                    .all(|export| export.unwrap().name != "cabi_realloc")
            );
        }
    }
    // Component encoding validates the reachable import and export ABI without realloc.
    assert!(compiled.component.is_some());
    Ok(())
}

#[test]
fn semantic_integer_arithmetic_wraps_at_each_width() -> Result<()> {
    for bits in [8, 16, 32, 64] {
        for signed in [false, true] {
            let ty = format!("builtin.integer {}i{bits}", if signed { "s" } else { "u" });
            let mask = u64::MAX >> (64 - bits);
            let normalize = |value: u64| {
                let value = value & mask;
                if signed && bits < 64 && value & (1 << (bits - 1)) != 0 {
                    value | !mask
                } else {
                    value
                }
            };
            let to_val = |value: u64| {
                let value = normalize(value);
                if bits == 64 {
                    wasmtime::Val::I64(value as i64)
                } else {
                    wasmtime::Val::I32(value as i32)
                }
            };
            for kind in ["Add", "Sub", "Mul"] {
                let mut program = mir_program(
                    &format!("({ty}, {ty}) -> ({ty})"),
                    &format!("x: {ty}, y: {ty}"),
                    &format!("value = perry.binary {kind} x, y : {ty}; perry.return value"),
                )?;
                assert_eq!(program.memory_bytes(), 0);
                for (x, y) in [
                    (mask, 1),
                    (1 << (bits - 1), mask),
                    (mask >> 1, 2),
                    (9007199254740993 & mask, 7),
                ] {
                    let expected = normalize(match kind {
                        "Add" => x.wrapping_add(y),
                        "Sub" => x.wrapping_sub(y),
                        _ => x.wrapping_mul(y),
                    });
                    let (actual, _) = program.call("run", &[to_val(x), to_val(y)]);
                    let actual = if bits == 64 {
                        actual[0].unwrap_i64() as u64
                    } else {
                        actual[0].unwrap_i32() as i64 as u64
                    };
                    let expected = if bits == 32 {
                        expected as i32 as i64 as u64
                    } else {
                        expected
                    };
                    assert_eq!(actual, expected, "{kind} {ty}: {x}, {y}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn integer_casts_use_source_signedness_and_wrap_narrowing() -> Result<()> {
    use wasmtime::Val::{I32, I64};
    for (from, to, input, expected) in [
        ("si8", "si64", I32(-1), -1i64),
        ("ui8", "si64", I32(255), 255),
        ("ui32", "ui64", I32(-1), 4294967295),
        ("si32", "ui64", I32(-1), -1),
        ("si64", "si8", I64(128), -128),
        ("ui64", "ui8", I64(-1), 255),
        ("ui64", "si32", I64(4294967295), -1),
        ("si8", "ui16", I32(-128), 65408),
    ] {
        let mut program = mir_program(
            &format!("(builtin.integer {from}) -> (builtin.integer {to})"),
            &format!("x: builtin.integer {from}"),
            &format!("value = perry.numeric_cast x : builtin.integer {to}; perry.return value"),
        )?;
        let (actual, _) = program.call("run", &[input]);
        let actual = if to.ends_with("64") {
            actual[0].unwrap_i64()
        } else {
            i64::from(actual[0].unwrap_i32())
        };
        assert_eq!(actual, expected, "{from} -> {to}");
        let literal = match input {
            I32(value) if from.starts_with('u') => (value as u32).to_string(),
            I32(value) => value.to_string(),
            I64(value) if from.starts_with('u') => (value as u64).to_string(),
            I64(value) => value.to_string(),
            _ => unreachable!(),
        };
        let mut folded = mir_program(
            &format!("() -> (builtin.integer {to})"),
            "",
            &format!(
                "x = builtin.constant <builtin.integer <{literal}: {from}>> : builtin.integer {from}; value = perry.numeric_cast x : builtin.integer {to}; perry.return value"
            ),
        )?;
        let (actual, _) = folded.call("run", &[]);
        let actual = if to.ends_with("64") {
            actual[0].unwrap_i64()
        } else {
            i64::from(actual[0].unwrap_i32())
        };
        assert_eq!(actual, expected, "folded {from} -> {to}");
    }
    Ok(())
}

#[test]
fn narrow_arithmetic_wraps_before_later_widening() -> Result<()> {
    let mut program = mir_program(
        "(builtin.integer si8, builtin.integer si8) -> (builtin.integer si64)",
        "x: builtin.integer si8, y: builtin.integer si8",
        "sum = perry.binary Add x, y : builtin.integer si8; value = perry.numeric_cast sum : builtin.integer si64; perry.return value",
    )?;
    let (result, _) = program.call("run", &[wasmtime::Val::I32(127), wasmtime::Val::I32(1)]);
    assert_eq!(result[0].unwrap_i64(), -128);
    Ok(())
}

#[test]
fn integer_comparisons_respect_signedness_at_wasm_widths() -> Result<()> {
    for (ty, input, expected) in [
        ("si32", wasmtime::Val::I32(-1), 0),
        ("ui32", wasmtime::Val::I32(-1), 1),
        ("si64", wasmtime::Val::I64(-1), 0),
        ("ui64", wasmtime::Val::I64(-1), 1),
    ] {
        let mut program = mir_program(
            &format!("(builtin.integer {ty}) -> (builtin.integer i1)"),
            &format!("x: builtin.integer {ty}"),
            &format!(
                "zero = builtin.constant <builtin.integer <0: {ty}>> : builtin.integer {ty}; result = perry.compare Gt x, zero : builtin.integer i1; perry.return result"
            ),
        )?;
        assert_eq!(
            program.call("run", &[input]).0[0].unwrap_i32(),
            expected,
            "{ty}"
        );
    }
    Ok(())
}
