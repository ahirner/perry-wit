//! Numeric constraints across calls, loops, ranges and bounded calling contexts.
use super::compile;
use crate::core_program::Program;
use crate::world::Compiler::Standard;
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
    let component = Standard.component(
        "package test:shared; world test { export narrow:func(x:u8,y:u8)->u8; export wide:func(x:u8,y:u8)->u32; export floating:func(x:f64,y:f64)->f64; }",
        "test",
        "shared.ts",
        source,
    )?;
    let engine = wasmtime::Engine::default();
    let (mut store, instance) = crate::world::instantiate(&engine, &component)?;
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
        let names = crate::world::function_names(&program.wasm)?;
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
    // Nullable conversions and nested layouts of an exact 64-bit value compile under default
    // and fully summarized limits.
    let exact = "parseInt(x===0?'9007199254740993':'bad')";
    let nullable = [
        "function f(n:number|null):number|undefined{return n===null?undefined:n;}",
        "function f(n:number|undefined):number|null{return n===undefined?null:n;}",
        "function f(n:number|null):number{return n===null?0.5:n;}",
        "function f(n:number):number|undefined{return n===0?undefined:n;}",
    ]
    .map(|helper| format!("{helper} export function run(x:number):string{{const n=f({exact});return JSON.stringify(n??0.5);}}"));
    let (a, b) = (
        "const a:S={inner:{a:w,b:0.5},opt:w};",
        "const b:S={inner:{a:w,b:w},opt:0.5};",
    );
    let nested = [format!("{a}{b}"), format!("{b}{a}")].map(|decls| format!("interface I{{a:number;b:number}} interface S{{inner:I;opt?:number}} export function run(x:number):string{{const w={exact};{decls}return JSON.stringify(a)+','+JSON.stringify(b);}}"));
    for source in nullable.iter().chain(&nested) {
        compile(source, "func(x:f64)->string")?;
        perry_mir::with_fact_limits(1, 1, || compile(source, "func(x:f64)->string"))?;
    }
    Ok(())
}
