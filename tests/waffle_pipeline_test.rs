//! Comprehensive integration test suite for the LLVM-free Perry HIR → WAFFLE SSA backend.

use std::time::Duration;

use anyhow::Result;
use perry_wit::compile_typescript_waffle;
use perry_wit::waffle_backend::WaffleCompileOptions;
use tokio::time::timeout;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Func, Instance, Module, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

fn make_async_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    Ok(Engine::new(&config)?)
}

#[derive(Default)]
struct WasiHostState {
    context: WasiCtx,
    table: ResourceTable,
}

impl WasiView for WasiHostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

fn make_wasi_linker(engine: &Engine) -> Result<Linker<WasiHostState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_primitive_assignments_and_arithmetic() -> Result<()> {
    let source = r#"
        export function run(input: number): number {
            let a = input + 10;
            let b = a * 2;
            b = b - 5;
            let c = b / 3;
            return c;
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "arithmetic.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.core.is_empty());
    assert!(compiled.waffle_ir.contains("f64add"));
    assert!(compiled.waffle_ir.contains("f64mul"));
    assert!(compiled.waffle_ir.contains("f64sub"));
    assert!(compiled.waffle_ir.contains("f64div"));

    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // (input: 5) => a = 15 => b = 30 => b = 25 => c = 25 / 3 = 8.3333...
    let res = run.call_async(&mut store, (5.0,)).await?;
    assert!((res.0 - (25.0 / 3.0)).abs() < 1e-6);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_branches_and_joins() -> Result<()> {
    let source = r#"
        export function run(input: number): number {
            let result = input;
            if (input > 10) {
                let doubled = result * 2;
                result = doubled;
            } else {
                let offset = 100;
                if (input > 0) {
                    let adjusted = result + offset;
                    result = adjusted;
                } else {
                    let fallback = offset;
                    result = fallback;
                }
            }
            return result + 1;
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "branches.ts", &WaffleCompileOptions::default())?;
    assert!(compiled.waffle_ir.contains("branch join"));

    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // input = 15 > 10 => result = 30 + 1 = 31
    let (res_then,) = run.call_async(&mut store, (15.0,)).await?;
    assert_eq!(res_then, 31.0);

    // input = 5 <= 10 => result = 105 + 1 = 106
    let (res_else,) = run.call_async(&mut store, (5.0,)).await?;
    assert_eq!(res_else, 106.0);

    let (res_nested_else,) = run.call_async(&mut store, (-1.0,)).await?;
    assert_eq!(res_nested_else, 101.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_while_loops_with_block_parameters() -> Result<()> {
    let source = r#"
        export function run(input: number): number {
            let total = 0;
            let i = 0;
            while (i < input) {
                total = total + i;
                i = i + 1;
            }
            return total;
        }
    "#;

    let compiled = compile_typescript_waffle(source, "loop.ts", &WaffleCompileOptions::default())?;
    assert!(compiled.waffle_ir.contains("loop header"));

    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // Sum of 0..4 = 0 + 1 + 2 + 3 + 4 = 10
    let (res,) = run.call_async(&mut store, (5.0,)).await?;
    assert_eq!(res, 10.0);

    // Sum of 0..9 = 45
    let (res10,) = run.call_async(&mut store, (10.0,)).await?;
    assert_eq!(res10, 45.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_intra_module_function_calls() -> Result<()> {
    let source = r#"
        export function run(input: number): number {
            let s = square(input);
            return s + square(2) + factorial(input) + even(input);
        }

        function square(x: number): number {
            return x * x;
        }

        function factorial(x: number): number {
            if (x <= 1) { return 1; }
            return x * factorial(x - 1);
        }

        function even(x: number): number {
            if (x <= 0) { return 1; }
            return odd(x - 1);
        }

        function odd(x: number): number {
            if (x <= 0) { return 0; }
            return even(x - 1);
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "intra_call.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.core.is_empty());

    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    let (res,) = run.call_async(&mut store, (5.0,)).await?;
    assert_eq!(res, 149.0);
    let (res_even,) = run.call_async(&mut store, (4.0,)).await?;
    assert_eq!(res_even, 45.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_evaluation_order_and_multi_arg_calls() -> Result<()> {
    let source = r#"
        function compute(first: number, second: number, third: number): number {
            return (first - second) * third;
        }

        export function run(input: number): number {
            return compute(input + 10, input * 2, input - 1);
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "eval_order.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // input = 4:
    // first = 4 + 10 = 14
    // second = 4 * 2 = 8
    // third = 4 - 1 = 3
    // compute(14, 8, 3) = (14 - 8) * 3 = 6 * 3 = 18
    let (res,) = run.call_async(&mut store, (4.0,)).await?;
    assert_eq!(res, 18.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_shadowing_and_binding_resolution() -> Result<()> {
    // 1. Parameter shadowing declared intrinsic is rejected
    let bad_shadow = r#"
        declare function waitFor(milliseconds: number): Promise<void>;
        export function run(waitFor: number): number {
            return waitFor + 1;
        }
    "#;
    let err1 =
        compile_typescript_waffle(bad_shadow, "bad_param.ts", &WaffleCompileOptions::default());
    assert!(err1.is_err());
    let err_msg1 = err1.unwrap_err().to_string();
    assert!(err_msg1.contains("shadows declared intrinsic"));

    // 2. Local variable shadowing declared intrinsic is rejected
    let bad_local = r#"
        declare function waitFor(milliseconds: number): Promise<void>;
        export function run(input: number): number {
            let waitFor = input * 2;
            return waitFor;
        }
    "#;
    let err2 =
        compile_typescript_waffle(bad_local, "bad_local.ts", &WaffleCompileOptions::default());
    assert!(err2.is_err());
    let err_msg2 = err2.unwrap_err().to_string();
    assert!(err_msg2.contains("illegally shadows declared intrinsic"));

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_rejects_uncovered_hir_explicitly() -> Result<()> {
    // Uncovered constructs (such as object literals without lowering support yet) must fail cleanly
    let unsupported = r#"
        export function run(input: number): number {
            let obj = { x: input };
            return obj.x;
        }
    "#;
    let res = compile_typescript_waffle(
        unsupported,
        "unsupported.ts",
        &WaffleCompileOptions::default(),
    );
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("Unsupported"));

    Ok(())
}

#[test]
fn test_waffle_rejects_module_initialization() {
    for initialization in ["hostDouble(123);", "let initial = hostDouble(123);"] {
        let source = format!(
            "declare function hostDouble(value: number): Promise<number>;\n\
             {initialization}\n\
             export function run(input: number): number {{ return input; }}"
        );
        for componentize in [false, true] {
            let options = WaffleCompileOptions {
                componentize,
                ..Default::default()
            };
            let error = compile_typescript_waffle(&source, "init.ts", &options).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Module initialization is unsupported")
            );
        }
    }
}

#[test]
fn test_waffle_boolean_returns_and_terminated_paths() -> Result<()> {
    let engine = Engine::default();
    let options = WaffleCompileOptions {
        componentize: false,
        ..Default::default()
    };
    for (body, expected) in [
        ("return true;", [1, 1]),
        (
            "if (input > 0) { return true; } else { return false; }",
            [0, 1],
        ),
        ("if (input > 0) { return true; } return false;", [0, 1]),
        ("while (input > 0) { return true; } return false;", [0, 1]),
        ("return false; let unreachable = 10;", [0, 0]),
    ] {
        let source = format!(
            "function helper(input: number): boolean {{ {body} }}\n\
             export function run(input: number): boolean {{ return helper(input); }}"
        );
        let compiled = compile_typescript_waffle(&source, "returns.ts", &options)?;
        let module = Module::new(&engine, compiled.core)?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[])?;
        let run = instance.get_typed_func::<f64, i32>(&mut store, "run")?;
        for (input, expected) in [0.0, 1.0].into_iter().zip(expected) {
            assert_eq!(run.call(&mut store, input)?, expected, "{body}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_async_p3_wait_and_suspension() -> Result<()> {
    let source = r#"
        declare function hostDouble(value: number): Promise<number>;
        declare function waitFor(milliseconds: number): Promise<void>;

        export async function run(input: number): Promise<number> {
            let total = input;
            let index = 0;
            while (index < 3) {
                if (index < 1) {
                    total = total + await hostDouble(input);
                } else {
                    total = await hostDouble(total) + index;
                }
                await waitFor(15);
                index = index + 1;
            }
            return total + index;
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "async_task.ts", &WaffleCompileOptions::default())?;
    assert!(compiled.uses_p3_clocks);
    assert!(compiled.waffle_ir.contains("await continuation"));

    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;

    let mut linker = make_wasi_linker(&engine)?;
    linker
        .root()
        .func_wrap_concurrent("host-double", |_, (value,): (f64,)| {
            Box::pin(async move { Ok((value * 2.0,)) })
        })?;

    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    let mut invocation = Box::pin(run.call_async(&mut store, (2.0,)));
    // Should take longer than 10ms because it waits 3 * 15ms = 45ms
    assert!(
        timeout(Duration::from_millis(10), &mut invocation)
            .await
            .is_err()
    );

    let (res,) = timeout(Duration::from_secs(5), invocation).await??;
    assert_eq!(res, 31.0);

    Ok(())
}

#[test]
fn test_waffle_boolean_await_preserves_result_and_locals() -> Result<()> {
    let source = r#"
        declare function flag(value: boolean): Promise<boolean>;
        export async function run(input: number): Promise<boolean> {
            let original = true;
            let result = await flag(false);
            let saved = await flag(original);
            if (input > 0) { return saved; }
            return result;
        }
    "#;
    let options = WaffleCompileOptions {
        componentize: false,
        ..Default::default()
    };
    let compiled = compile_typescript_waffle(source, "await_bool.ts", &options)?;
    let engine = Engine::default();
    let module = Module::new(&engine, compiled.core)?;
    let mut store = Store::new(&engine, ());
    let flag = Func::wrap(&mut store, |value: i32| value);
    let instance = Instance::new(&mut store, &module, &[flag.into()])?;
    let run = instance.get_typed_func::<f64, i32>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, 0.0)?, 0);
    assert_eq!(run.call(&mut store, 1.0)?, 1);
    Ok(())
}
