//! Binary structure and behavior of optimized WIT-constrained programs.
use anyhow::Result;
use wasmparser::{Parser, Payload};
use wasmtime::{
    Store,
    component::{Component, Instance, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

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
    let stderr = if signature.starts_with("async ") {
        "import wasi:cli/stderr@0.3.0;"
    } else {
        ""
    };
    let (resolve, package) = perry_wit::component::wit::resolve_source(
        "optimization.wit",
        &format!("package test:optimization; world test {{ {stderr} export run: {signature}; }}"),
    )?;
    let world = resolve.select_world(&[package], Some("test"))?;
    Ok(perry_wit::compile_for_world(
        source,
        "optimization.ts",
        &resolve,
        world,
        true,
    )?)
}

struct WasiHost {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for WasiHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

async fn instantiate_async(bytes: &[u8]) -> Result<(Store<WasiHost>, Instance)> {
    let engine = crate::engine::async_engine()?;
    let component = Component::new(&engine, bytes)?;
    let mut store = Store::new(
        &engine,
        WasiHost {
            ctx: WasiCtx::builder().build(),
            table: ResourceTable::new(),
        },
    );
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
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
                        assert!(
                            !matches!(
                                op,
                                wasmparser::Operator::I32Shl | wasmparser::Operator::I32ShrS
                            ),
                            "{wit}: use sign-extension instructions"
                        );
                        if matches!(wit, "s32" | "u32" | "s64" | "u64") {
                            assert!(
                                !matches!(op, wasmparser::Operator::Call { .. }),
                                "{wit}: exported arithmetic must not forward"
                            );
                        }
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
    use wasmtime::component::Val;
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
            let (mut store, instance) =
                instantiate_async(&compiled.component.unwrap().stripped).await?;
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
    use wasmtime::component::Val;
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
        let (mut store, instance) =
            instantiate_async(&compiled.component.unwrap().stripped).await?;
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
fn numeric_constraints_keep_each_call_result_independent() -> Result<()> {
    use pliron::{
        builtin::{
            op_interfaces::{OneRegionInterface, SymbolOpInterface},
            ops::{FuncOp, ModuleOp},
            types::{FP64Type, IntegerType, Signedness},
        },
        context::Context,
        irfmt::parsers::spaced,
        linked_list::ContainsLinkedList,
        operation::Operation,
        parsable::parse_from_str,
        printable::Printable,
    };
    let ctx = &mut Context::new();
    let root = parse_from_str(
        spaced(Operation::top_level_parser()),
        ctx,
        r#"builtin.module @multiple_results { ^module():
            builtin.func @helper: builtin.function <(builtin.fp64, builtin.integer i1) -> (builtin.integer i1, builtin.fp64, builtin.fp64)> {
              ^entry(x: builtin.fp64, recurse: builtin.integer i1):
                perry.cond_br if recurse ^recursive() else ^base()
              ^recursive():
                no = builtin.constant <builtin.integer <0: i1>> : builtin.integer i1;
                status, fraction, sum = perry.call helper (x, no) : builtin.integer i1, builtin.fp64, builtin.fp64;
                perry.br ^done(status, fraction, sum)
              ^base():
                yes = builtin.constant <builtin.integer <1: i1>> : builtin.integer i1;
                two = builtin.constant <builtin.double 2.0> : builtin.fp64;
                half = perry.binary Div x, two : builtin.fp64;
                doubled = perry.binary Add x, x : builtin.fp64;
                perry.br ^done(yes, half, doubled)
              ^done(ok: builtin.integer i1, divided: builtin.fp64, added: builtin.fp64):
                perry.return ok, divided, added
            };
            builtin.func @run: builtin.function <(builtin.fp64, builtin.integer i1) -> (builtin.integer i1, builtin.fp64, builtin.fp64)> {
              ^entry(x: builtin.fp64, recurse: builtin.integer i1):
                status, fraction, sum = perry.call helper (x, recurse) : builtin.integer i1, builtin.fp64, builtin.fp64;
                perry.return status, fraction, sum
            }
        }"#,
    )
    .map_err(|error| anyhow::anyhow!("{}", error.disp(ctx)))?;
    let module = Operation::get_op::<ModuleOp>(root, ctx).unwrap();
    let run = module
        .get_region(ctx)
        .deref(ctx)
        .iter(ctx)
        .flat_map(|block| block.deref(ctx).iter(ctx))
        .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
        .find(|function| function.get_symbol_name(ctx) == "run".try_into().unwrap())
        .unwrap();
    let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
    let byte = IntegerType::get(ctx, 8, Signedness::Unsigned).into();
    let wide = IntegerType::get(ctx, 32, Signedness::Unsigned).into();
    let float = FP64Type::get(ctx).into();
    perry_mir::facts::constrain(ctx, &run, vec![byte, boolean], vec![boolean, float, wide]);
    perry_mir::calls::set_exported(ctx, &run);
    let mut analyses = perry_mir::pipeline::canonicalize(root, ctx)
        .map_err(|error| anyhow::anyhow!("{}", error.disp(ctx)))?;
    let implementation = perry_mir::facts::implementation(ctx, &run).to_string();
    let bytes = perry_codegen::compile(ctx, &module, &perry_codegen::Exports::Raw, &mut analyses)?;
    let mut program = crate::core_program::Program::from_wasm(bytes);
    for recurse in [0, 1] {
        let results = program
            .call(
                &implementation,
                &[wasmtime::Val::I32(255), wasmtime::Val::I32(recurse)],
            )
            .0;
        assert_eq!(results[0].unwrap_i32(), 1);
        assert_eq!(results[1].unwrap_f64(), 127.5);
        assert_eq!(results[2].unwrap_i32(), 510);
        let results = program
            .call(
                "run",
                &[
                    wasmtime::Val::F64(0.25f64.to_bits()),
                    wasmtime::Val::I32(recurse),
                ],
            )
            .0;
        assert_eq!(results[0].unwrap_i32(), 1);
        assert_eq!(results[1].unwrap_f64(), 0.125);
        assert_eq!(results[2].unwrap_f64(), 0.5);
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
        "export function run(flag:boolean):number { try { const values=[10]; const index=200; if(flag && index<values.length){const item=values[index];return item===undefined?7:item;} return index; } catch { return 13; } }",
        "func(flag:bool)->s8",
    )?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) =
        crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
    let run = instance.get_typed_func::<(bool,), (i8,)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, (true,))?, (7,));
    assert_eq!(run.call(&mut store, (false,))?, (-56,));
    for initializer in ["{ index: [1].length }", "{ index: supplied }"] {
        let source = format!(
            "type Position = {{index:number}}; \
             function read(position:Position, values:number[], again:boolean):number {{ \
               if (again) return read(position, values, false); \
               const index=position.index; \
               if(index<values.length){{const value=values[index];return value===undefined?-1:value;}} \
               return -2; \
             }} \
             export function run(supplied:number, again:boolean):number {{ \
               const positions=[{initializer}]; const position=positions[0]; \
               if(position===undefined)return -3; return read(position,[11,22,33],again); \
             }}"
        );
        let mut program = crate::core_program::Program::new(&source);
        let mut retained_helper = false;
        for payload in Parser::new(0).parse_all(&program.wasm) {
            if let Payload::CustomSection(section) = payload?
                && let wasmparser::KnownCustom::Name(names) = section.as_known()
            {
                for subsection in names {
                    if let wasmparser::Name::Function(names) = subsection? {
                        for entry in names {
                            let name = entry?.name;
                            retained_helper |= name == "read";
                            assert!(
                                !name.starts_with("perry_gc_")
                                    && !name.starts_with("perry_roots_")
                                    && name != "perry_frame_free",
                                "bounded allocations retain {name}"
                            );
                        }
                    }
                }
            }
        }
        assert!(
            retained_helper,
            "inspect the retained helper and named runtime roots"
        );
        for (index, expected) in [(-1.0, -1.0), (0.25, -1.0), (1.0, 22.0), (3.0, -2.0)] {
            for again in [0, 1] {
                let result = program
                    .call(
                        "run",
                        &[
                            wasmtime::Val::F64(f64::to_bits(index)),
                            wasmtime::Val::I32(again),
                        ],
                    )
                    .0[0]
                    .unwrap_f64();
                assert_eq!(
                    result,
                    if initializer.contains("supplied") {
                        expected
                    } else {
                        22.0
                    },
                    "{initializer}, {index}, {again}"
                );
            }
        }
        let warmed_memory = program.memory_bytes();
        for _ in 0..1000 {
            assert_eq!(
                program
                    .call(
                        "run",
                        &[wasmtime::Val::F64(1.0f64.to_bits()), wasmtime::Val::I32(1)]
                    )
                    .0[0]
                    .unwrap_f64(),
                22.0
            );
        }
        assert_eq!(
            program.memory_bytes(),
            warmed_memory,
            "task regions must be reused across invocations"
        );
    }
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
        assert_eq!(functions, 1, "export the implementation directly");
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
        "function check(a:number):number { if(a<0)throw new Error('negative'); return a; } export function run(a:number):number { try { return check(a); } catch { return -1; } }",
        "let count=0; export function run(a:number):number { try { count+=a; return count; } catch { return -1; } }",
    ] {
        let compiled = compile(source, "func(a:f64)->f64")?;
        let mut exports = Vec::new();
        for payload in Parser::new(0).parse_all(&compiled.core) {
            let payload = payload?;
            assert!(
                !matches!(payload, Payload::StartSection { .. }),
                "constant helper initialization must be resolved before instantiation"
            );
            if let Payload::CustomSection(section) = &payload
                && let wasmparser::KnownCustom::Name(names) = section.as_known()
            {
                for subsection in names {
                    if let wasmparser::Name::Function(names) = subsection? {
                        for entry in names {
                            assert_ne!(entry?.name, "__wasm_apply_data_relocs");
                        }
                    }
                }
            }
            if let Payload::ExportSection(section) = payload {
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

#[test]
fn scalar_core_is_one_direct_function_with_and_without_names() -> Result<()> {
    let compiled = compile(
        "export function run(a:number,b:number):number { return a+b; }",
        "func(a:s32,b:s32)->s32",
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
    for bytes in [&compiled.core, &stripped] {
        let mut functions = 0;
        let mut bodies = 0;
        for payload in Parser::new(0).parse_all(bytes) {
            match payload? {
                Payload::FunctionSection(section) => functions = section.count(),
                Payload::CodeSectionEntry(body) => {
                    bodies += 1;
                    let ops = body
                        .get_operators_reader()?
                        .into_iter()
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    assert!(
                        matches!(
                            ops.as_slice(),
                            [
                                wasmparser::Operator::LocalGet { local_index: 0 },
                                wasmparser::Operator::LocalGet { local_index: 1 },
                                wasmparser::Operator::I32Add,
                                wasmparser::Operator::End,
                            ]
                        ),
                        "{ops:?}"
                    );
                }
                Payload::MemorySection(_) | Payload::ImportSection(_) => {
                    panic!("scalar arithmetic needs no runtime")
                }
                Payload::ExportSection(section) => {
                    let exports = section
                        .into_iter()
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    assert_eq!(exports.len(), 1);
                    assert_eq!((exports[0].name, exports[0].index), ("run", 0));
                }
                _ => {}
            }
        }
        assert_eq!((functions, bodies), (1, 1));
    }
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
            ("a:number".to_owned(), format!("func(a:{wit})->{wit}"))
        } else {
            (
                "a:number,b:number".to_owned(),
                format!("func(a:{wit},b:{wit})->{wit}"),
            )
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
                        wasmparser::Operator::I32Extend8S if wit == "s8" => extensions += 1,
                        wasmparser::Operator::I32Extend16S if wit == "s16" => extensions += 1,
                        wasmparser::Operator::I32Shl | wasmparser::Operator::I32ShrS => {
                            panic!("use sign extension")
                        }
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
fn async_dispatch_roots_only_constructed_frame_handlers() -> Result<()> {
    for (body, combinator) in [
        ("return await value(n);", false),
        (
            "const values = await Promise.all([value(n), Promise.resolve(n)]); return values[0] + values[1];",
            true,
        ),
    ] {
        let compiled = compile(
            &format!(
                "async function value(n:number):Promise<number> {{ return n+1; }} export async function run(n:number):Promise<number> {{ {body} }}"
            ),
            "async func(n:f64)->f64",
        )?;
        let wat = wasmprinter::print_bytes(&compiled.core)?;
        assert_eq!(wat.contains("(func $perry_combinator_step "), combinator);
    }
    Ok(())
}
