//! Binary structure and behavior of optimized WIT-constrained programs.
use anyhow::Result;
use wasmparser::{Parser, Payload};
mod asynchronous;

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
    use wasmtime::Val::{I32, I64};
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
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(program.memory_bytes(), 0);
    assert_eq!(
        program.call("wrap-byte", &[I32(250), I32(10)]).0[0].unwrap_i32(),
        4
    );
    assert_eq!(
        program.call("widen-s8", &[I32(127), I32(1)]).0[0].unwrap_i32(),
        -128
    );
    assert_eq!(
        program.call("big", &[I64(9007199254740992), I64(1)]).0[0].unwrap_i64(),
        9007199254740993
    );
    Ok(())
}

#[test]
fn exact_64_bit_integers_and_parse_int_preserve_precision() -> Result<()> {
    let mut roundtrip = crate::core_program::Program::from_wasm(
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
        &[
            wasmtime::Val::I64(9007199254740993),
            wasmtime::Val::I64(-9007199254740993),
        ],
    );
    assert_eq!(res[0].unwrap_i32(), 1);

    let mut parse_int = crate::core_program::Program::from_wasm(
        compile(
            r#"
interface Box { raw: string }
interface NumRec { x: number }
function fwd(v: number): number { return v; }
function choose(flag: number): string { return flag > 0 ? "9007199254740993" : "7"; }
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
    const rec: NumRec = { x: parseInt(s) };
    const numRecord = JSON.stringify(rec.x + 1) === "9007199254740994";
    const arr = [parseInt(s)];
    const numArray = JSON.stringify(arr[0] + 1) === "9007199254740994";
    const opt: number | undefined = flag > 0 ? parseInt(s) : undefined;
    const numOptional = JSON.stringify(opt === undefined ? 0 : opt + 1) === "9007199254740994";
    const br = flag > 0 ? parseInt(s) : 1;
    const numBranch = JSON.stringify(br + 1) === "9007199254740994";
    const numRem = (parseInt(s) % 10) === 3;
    const numMath =
        JSON.stringify(Math.trunc(parseInt(s)) + 1) === "9007199254740994" &&
        JSON.stringify(Math.abs(parseInt("-9007199254740993")) + 1) === "9007199254740994" &&
        JSON.stringify(Math.max(parseInt(s), 10) + 1) === "9007199254740994";
    return eq && mul && cmp && helper && overflow && radixWrap && runtimeText && runtimeRadix && runtimeJson && nonJsWs && numRecord && numArray && numOptional && numBranch && numRem && numMath ? 1 : 0;
}
"#,
            "func(flag:s32)->s32",
        )?
        .core,
    );
    assert_eq!(
        parse_int.call("run", &[wasmtime::Val::I32(1)]).0[0].unwrap_i32(),
        1
    );
    Ok(())
}

#[test]
fn bounded_context_specialization_is_sound_and_order_invariant() -> Result<()> {
    for (case_name, helper) in [
        (
            "inline_compare",
            "function helper(a:number,b:number):number { const sum = a+b; return sum > 200 ? sum : -sum; }",
        ),
        (
            "delegated_compare",
            "function classify(v:number):number { return v > 200 ? v : -v; }\nfunction helper(a:number,b:number):number { const sum = a+b; return classify(sum); }",
        ),
        (
            "divide_after_add",
            "function helper(a:number,b:number):number { const sum = a+b; return sum / 2; }",
        ),
    ] {
        let target = "export function run(a:number,b:number):number { let acc = 0; for (let i = 0; i < b; i = i + 1) { acc = acc + i; } return acc < 0 ? 0 : helper(a,b); }";
        let mut others = String::new();
        let mut extra_exports = String::new();
        for a in ["u8", "s8", "u16", "s16", "u32", "s32"] {
            for b in ["u8", "s8", "u16", "s16", "u32", "s32"] {
                if a == "u8" && b == "u8" {
                    continue;
                }
                let id = format!("e{}", extra_exports.len());
                others += &format!(
                    "export function {id}(a:number,b:number):number {{ return helper(a,b); }}\n"
                );
                extra_exports += &format!("; export {id}: func(a:{a},b:{b})->u8");
            }
        }
        let mut baseline = None;
        for (order, source, signature, expect_summary) in [
            (
                "single",
                format!("{helper}\n{target}"),
                "func(a:u8,b:u8)->u8".to_owned(),
                false,
            ),
            (
                "target_first",
                format!("{helper}\n{target}\n{others}"),
                format!("func(a:u8,b:u8)->u8{extra_exports}"),
                true,
            ),
            (
                "target_last",
                format!("{helper}\n{others}\n{target}"),
                format!("func(a:u8,b:u8)->u8{extra_exports}"),
                true,
            ),
        ] {
            let compiled = compile(&source, &signature)?;
            assert!(
                !compiled.allocation_report.contains("range_widenings=0"),
                "{}",
                compiled.allocation_report
            );
            if expect_summary {
                assert!(
                    !compiled.allocation_report.contains("summarized_contexts=0"),
                    "{}",
                    compiled.allocation_report
                );
            }
            let mut prog = crate::core_program::Program::from_wasm(compiled.core);
            let actual = prog
                .call("run", &[wasmtime::Val::I32(250), wasmtime::Val::I32(10)])
                .0[0]
                .unwrap_i32()
                & 0xff;
            if let Some(expected) = baseline {
                assert_eq!(
                    actual, expected,
                    "context order case `{case_name}` (`{order}`) mismatch"
                );
            } else {
                baseline = Some(actual);
            }
        }
    }
    Ok(())
}

#[test]
fn collection_mutation_during_iteration_and_property_presence_preserve_semantics() -> Result<()> {
    let mut program = crate::core_program::Program::from_wasm(
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

    const emptyTypedMap = new Map<string, number>();
    emptyTypedMap.set("k", 7);
    const typedMapOk = emptyTypedMap.get("k") === 7 && emptyTypedMap.size === 1;

    const s3 = new Set([1, 2]);
    let s3Total = 0;
    s3.forEach((v) => {
        s3Total = s3Total + v;
        if (v === 1) {
            s3.clear();
            s3.add(9);
        }
    });

    const liveSet = new Set<number>([1, 2]);
    const setIter = liveSet.values();
    liveSet.delete(2);
    liveSet.add(4);
    let liveSetTotal = 0;
    for (const v of setIter) {
        liveSetTotal = liveSetTotal + v;
    }

    const liveSetK = new Set<number>([1, 2]);
    const setKeyIter = liveSetK.keys();
    liveSetK.delete(2);
    liveSetK.add(4);
    let liveSetKeyTotal = 0;
    for (const k of setKeyIter) {
        liveSetKeyTotal = liveSetKeyTotal + k;
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
        typedMapOk &&
        s3Total === 10 &&
        liveSetTotal === 5 &&
        liveSetKeyTotal === 5 &&
        liveSetEntTotal === 10 &&
        liveMapValTotal === 50 &&
        liveMapKeyTotal === 5 &&
        liveMapEntTotal === 55 &&
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

    let mut churn = crate::core_program::Program::from_wasm(
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
    let (res_128, fuel_128) = churn.call("run", &[wasmtime::Val::I32(128)]);
    let (res_512, fuel_512) = churn.call("run", &[wasmtime::Val::I32(512)]);
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
    assert!(
        !compiled
            .allocation_report
            .contains("parse_stringify_rewrites=0"),
        "{}",
        compiled.allocation_report
    );
    assert!(
        !compiled
            .allocation_report
            .contains("parse_stringify_missed=0"),
        "{}",
        compiled.allocation_report
    );
    let mut program = crate::core_program::Program::from_wasm(compiled.core);
    assert_eq!(
        program
            .call("run", &[wasmtime::Val::F64(0.0f64.to_bits())])
            .0[0]
            .unwrap_i32(),
        1
    );
    Ok(())
}
