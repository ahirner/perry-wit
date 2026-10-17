//! Numeric constraints across calls, loops, ranges and bounded calling contexts.
use super::{compile, inspect_wasm};
use crate::core_program::Program;
use anyhow::Result;
use wasmtime::Val;

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
    let mut program = Program::from_wasm(compiled.core);
    assert_eq!(program.call("run", &[Val::I32(20)]).0[0].unwrap_i32(), 210);
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
    perry_mir::constrain_signature(ctx, &run, vec![byte, boolean], vec![boolean, float, wide]);
    perry_mir::set_exported(ctx, &run);
    let mut analyses = perry_mir::canonicalize(root, ctx)
        .map_err(|error| anyhow::anyhow!("{}", error.disp(ctx)))?;
    let implementation = perry_mir::specialized_implementation(ctx, &run).to_string();
    let bytes = perry_codegen::compile(ctx, &module, &perry_codegen::Exports::Raw, &mut analyses)?;
    let mut program = Program::from_wasm(bytes);
    for recurse in [0, 1] {
        let results = program
            .call(&implementation, &[Val::I32(255), Val::I32(recurse)])
            .0;
        assert_eq!(results[0].unwrap_i32(), 1);
        assert_eq!(results[1].unwrap_f64(), 127.5);
        assert_eq!(results[2].unwrap_i32(), 510);
        let results = program
            .call("run", &[Val::F64(0.25f64.to_bits()), Val::I32(recurse)])
            .0;
        assert_eq!(results[0].unwrap_i32(), 1);
        assert_eq!(results[1].unwrap_f64(), 0.125);
        assert_eq!(results[2].unwrap_f64(), 0.5);
    }
    Ok(())
}

#[test]
fn fractional_operations_and_unconstrained_callers_keep_number_semantics() -> Result<()> {
    let compiled = compile(
        "export function run(a:number,b:number):number { return a/b + a%b; }",
        "func(a:s32,b:s32)->f64",
    )?;
    let mut program = Program::from_wasm(compiled.core);
    for (a, b) in [(5, 2), (-5, 2), (7, -3)] {
        assert_eq!(
            program.call("run", &[Val::I32(a), Val::I32(b)]).0[0].unwrap_f64(),
            a as f64 / b as f64 + a as f64 % b as f64
        );
    }
    let source = include_str!("../../fixtures/optimization/shared.ts");
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
        Program::new("export function run(x:number):number { return x+0.5; }").number("run", 0.25),
        0.75
    );
    Ok(())
}

#[test]
fn range_queries_follow_actual_wrap_points() -> Result<()> {
    let mut raw = Program::new(
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
        let mut program = Program::new(&source);
        let names = inspect_wasm(&program.wasm)?.function_names;
        assert!(
            names.contains(&"read"),
            "inspect the retained helper and named runtime roots"
        );
        for name in names {
            assert!(
                !name.starts_with("perry_gc_")
                    && !name.starts_with("perry_roots_")
                    && name != "perry_frame_free",
                "bounded allocations retain {name}"
            );
        }
        let supplied = initializer.contains("supplied");
        for (index, expected) in [(-1.0, -1.0), (0.25, -1.0), (1.0, 22.0), (3.0, -2.0)] {
            for again in [0, 1] {
                let result = program
                    .call("run", &[Val::F64(f64::to_bits(index)), Val::I32(again)])
                    .0[0]
                    .unwrap_f64();
                assert_eq!(
                    result,
                    if supplied { expected } else { 22.0 },
                    "{initializer}, {index}, {again}"
                );
            }
        }
        let warmed_memory = program.memory_bytes();
        for _ in 0..1000 {
            assert_eq!(
                program
                    .call("run", &[Val::F64(1.0f64.to_bits()), Val::I32(1)])
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

/// Context-count bounds per arity are asserted precisely in perry-mir's facts tests;
/// this checks that forced context summarization via `with_fact_limits(1, 1, ...)`
/// records `summarized_contexts` and `range_widenings` and preserves helper semantics
/// regardless of function declaration order.
#[test]
fn bounded_context_specialization_is_sound_and_order_invariant() -> Result<()> {
    let other = "export function other(a:number,b:number):number { return helper(a,b); }";
    let target = "export function run(a:number,b:number):number { let acc = 0; for (let i = 0; i < b; i = i + 1) { acc = acc + i; } return acc < 0 ? 0 : helper(a,b); }";
    for (case_name, helper, run_sig, call_args, expected_val) in [
        (
            "inline_compare",
            "function helper(a:number,b:number):number { const sum = a+b; return sum > 200 ? sum : -sum; }",
            "func(a:u8,b:u8)->u8",
            (250, 10),
            252,
        ),
        (
            "mixed_width_wrap",
            "function helper(a:number,b:number):number { const sum = a+a; return b === 0 ? (sum > 250 ? sum : -sum) : b; }",
            "func(a:u8,b:u16)->u8",
            (250, 0),
            12,
        ),
        (
            "mixed_width_widen_return",
            "function helper(a:number,b:number):number { const sum = a+a; return b === 0 ? (sum > 250 ? sum : -sum) : b; }",
            "func(a:u8,b:u8)->u16",
            (250, 0),
            500,
        ),
    ] {
        let signature = format!("{run_sig}; export other: func(a:s16,b:s16)->u8");
        let mask = if run_sig.ends_with("->u16") {
            0xffff
        } else {
            0xff
        };
        for (order_name, source) in [
            ("other_first", format!("{helper}\n{other}\n{target}")),
            ("target_first", format!("{target}\n{other}\n{helper}")),
        ] {
            for summarize in [false, true] {
                let compiled = if summarize {
                    perry_mir::with_fact_limits(1, 1, || compile(&source, &signature))?
                } else {
                    compile(&source, &signature)?
                };
                let report = &compiled.allocation_report;
                assert!(!report.contains("range_widenings=0"), "{report}");
                if summarize {
                    assert!(!report.contains("summarized_contexts=0"), "{report}");
                }
                let mut prog = Program::from_wasm(compiled.core);
                let actual = prog
                    .call("run", &[Val::I32(call_args.0), Val::I32(call_args.1)])
                    .0[0]
                    .unwrap_i32()
                    & mask;
                assert_eq!(
                    actual, expected_val,
                    "context summarization case `{case_name}` ({order_name}, summarize={summarize}) mismatch"
                );
            }
        }
    }
    Ok(())
}
