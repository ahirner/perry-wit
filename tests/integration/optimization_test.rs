//! Binary structure and behavior of optimized WIT-constrained programs.
use anyhow::Result;
use wasmparser::{Parser, Payload};
mod asynchronous;

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
                        let exports: Vec<_> = section
                            .into_iter()
                            .map(|export| export.unwrap().name.to_owned())
                            .collect();
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
                                        wasmparser::Operator::LocalGet { local_index: 0 },
                                        wasmparser::Operator::LocalGet { local_index: 1 },
                                        wasmparser::Operator::I32Add,
                                        wasmparser::Operator::End,
                                    ]
                                ),
                                "{ops:?}"
                            );
                        }
                        for op in ops {
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
        let mut program = crate::core_program::Program::from_wasm(compiled.core);
        assert_eq!(
            program
                .call("run", &[wasmtime::Val::I32(-1), wasmtime::Val::I32(-1)])
                .0[0]
                .unwrap_f64(),
            expected,
            "{source}"
        );
    }
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
        "export function run(a:number,b:number):number { return [a,b].length+a; }",
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




