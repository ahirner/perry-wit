//! Comprehensive integration test suite for the LLVM-free Perry HIR → WAFFLE SSA backend.

use std::time::Duration;

use anyhow::Result;
use perry_wit::compile_typescript_waffle;
use perry_wit::waffle_backend::WaffleCompileOptions;
use tokio::time::timeout;
use wasmtime::component::{Component, Linker, ResourceTable, Val};
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

#[test]
fn test_waffle_boolean_comparisons() -> Result<()> {
    let options = WaffleCompileOptions {
        componentize: false,
        ..Default::default()
    };
    let engine = Engine::default();
    for (operator, expected) in [
        ("===", [1, 0, 0, 1]),
        ("==", [1, 0, 0, 1]),
        ("!==", [0, 1, 1, 0]),
        ("!=", [0, 1, 1, 0]),
        ("<", [0, 1, 0, 0]),
        ("<=", [1, 1, 0, 1]),
        (">", [0, 0, 1, 0]),
        (">=", [1, 0, 1, 1]),
    ] {
        let source = format!(
            "export function run(input: number, left: boolean, right: boolean): boolean {{
                if (left {operator} right) {{ return true; }}
                return false;
            }}"
        );
        let compiled = compile_typescript_waffle(&source, "compare.ts", &options)?;
        let module = Module::new(&engine, compiled.core)?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[])?;
        let run = instance.get_typed_func::<(f64, i32, i32), i32>(&mut store, "run")?;
        for ((left, right), expected) in [(0, 0), (0, 1), (1, 0), (1, 1)].into_iter().zip(expected)
        {
            assert_eq!(
                run.call(&mut store, (0.0, left, right))?,
                expected,
                "{operator}"
            );
        }
    }
    for comparison in ["true === 1", "false < 0"] {
        let source = format!(
            "export function run(input: number): number {{
                if ({comparison}) {{ return 1; }}
                return 0;
            }}"
        );
        let error = compile_typescript_waffle(&source, "mixed_compare.ts", &options).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Unsupported comparison operand types")
        );
    }
    Ok(())
}

#[test]
fn test_waffle_stream_componentization_is_explicitly_unsupported() -> Result<()> {
    let source = "export function run(input: ByteStream): number { return 42; }";
    let error = compile_typescript_waffle(source, "stream.ts", &WaffleCompileOptions::default())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ByteStream componentization is unsupported")
    );

    let options = WaffleCompileOptions {
        componentize: false,
        ..Default::default()
    };
    let compiled = compile_typescript_waffle(source, "stream.ts", &options)?;
    let engine = Engine::default();
    Module::new(&engine, compiled.core)?;
    assert!(compiled.component.is_none());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_component_entry_signatures() -> Result<()> {
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;
    for (source, params, expected) in [
        (
            "export function run(): number { return 42; }",
            vec![],
            vec![Val::Float64(42.0)],
        ),
        (
            "export function run(first: number, second: number): number { return first - second; }",
            vec![Val::Float64(7.0), Val::Float64(2.0)],
            vec![Val::Float64(5.0)],
        ),
        (
            "export function run(input: number): void { return; }",
            vec![Val::Float64(7.0)],
            vec![],
        ),
        (
            "export async function run(): Promise<void> { return; }",
            vec![],
            vec![],
        ),
        (
            "export function run(input: number, flag: boolean): boolean { return flag; }",
            vec![Val::Float64(7.0), Val::Bool(true)],
            vec![Val::Bool(true)],
        ),
    ] {
        let compiled =
            compile_typescript_waffle(source, "signature.ts", &WaffleCompileOptions::default())?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, WasiHostState::default());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_func(&mut store, "run").unwrap();
        let mut results = vec![Val::Bool(false); expected.len()];
        run.call_async(&mut store, &params, &mut results).await?;
        assert_eq!(results, expected, "{source}");
    }
    for parameter_count in [16, 17] {
        let params = (0..parameter_count)
            .map(|index| format!("arg{index}: number"))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("export function run({params}): number {{ return arg15; }}");
        let compiled =
            compile_typescript_waffle(&source, "many_params.ts", &WaffleCompileOptions::default());
        if parameter_count == 16 {
            Component::new(&engine, compiled?.component.unwrap())?;
        } else {
            assert!(
                compiled
                    .unwrap_err()
                    .to_string()
                    .contains("more than 16 flattened parameters")
            );
        }
    }
    let error = compile_typescript_waffle(
        "export function run(input: number, stream: ByteStream): number { return input; }",
        "stream_param.ts",
        &WaffleCompileOptions::default(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ByteStream componentization is unsupported")
    );
    Ok(())
}

#[test]
fn test_waffle_rejects_intrinsics_without_component_wiring() -> Result<()> {
    let engine = Engine::default();
    for (declaration, body, name) in [
        (
            "declare function foo(value: number): number;",
            "return foo(input);",
            "foo",
        ),
        (
            "declare function flag(): Promise<boolean>;",
            "let value = await flag(); return input;",
            "flag",
        ),
        (
            "declare function unused(): void;",
            "return input;",
            "unused",
        ),
        (
            "declare function byteAt(index: number): number;",
            "return byteAt(input);",
            "byteAt",
        ),
        (
            "declare function readChunk(stream: ByteStream): Promise<number>;",
            "return input;",
            "readChunk",
        ),
    ] {
        let source = format!(
            "{declaration}\nexport async function run(input: number): Promise<number> {{ {body} }}"
        );
        let error =
            compile_typescript_waffle(&source, "intrinsic.ts", &WaffleCompileOptions::default())
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("Intrinsic '{name}' is unsupported in components"))
        );

        let options = WaffleCompileOptions {
            componentize: false,
            ..Default::default()
        };
        let compiled = compile_typescript_waffle(&source, "intrinsic.ts", &options)?;
        Module::new(&engine, compiled.core)?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_try_catch_finally_ordering() -> Result<()> {
    // 1. Basic try-catch
    let source1 = r#"
        export function run(input: number): number {
            let val = input;
            try {
                if (val > 10) {
                    throw val * 2;
                }
                val = val + 1;
            } catch (err) {
                val = err + 5;
            }
            return val;
        }
    "#;
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;

    let compiled1 =
        compile_typescript_waffle(source1, "try_catch.ts", &WaffleCompileOptions::default())?;
    let component1 = Component::new(&engine, compiled1.component.unwrap())?;
    let mut store1 = Store::new(&engine, WasiHostState::default());
    let instance1 = linker.instantiate_async(&mut store1, &component1).await?;
    let run1 = instance1.get_typed_func::<(f64,), (f64,)>(&mut store1, "run")?;

    // input = 5: no throw -> 5 + 1 = 6
    let (res_ok,) = run1.call_async(&mut store1, (5.0,)).await?;
    assert_eq!(res_ok, 6.0);
    // input = 20: throw 40 -> catch: 40 + 5 = 45
    let (res_err,) = run1.call_async(&mut store1, (20.0,)).await?;
    assert_eq!(res_err, 45.0);

    // 2. Return inside try with finally execution
    let source2 = r#"
        export function run(input: number): number {
            try {
                return input + 10;
            } finally {
                let dummy = 999;
            }
        }
    "#;
    let compiled2 =
        compile_typescript_waffle(source2, "try_finally.ts", &WaffleCompileOptions::default())?;
    let component2 = Component::new(&engine, compiled2.component.unwrap())?;
    let mut store2 = Store::new(&engine, WasiHostState::default());
    let instance2 = linker.instantiate_async(&mut store2, &component2).await?;
    let run2 = instance2.get_typed_func::<(f64,), (f64,)>(&mut store2, "run")?;

    let (res_fin,) = run2.call_async(&mut store2, (7.0,)).await?;
    assert_eq!(res_fin, 17.0);

    // 3. Nested try-catch-finally with re-throw
    let source3 = r#"
        export function run(input: number): number {
            let result = input;
            try {
                result = result + 1;
                try {
                    result = result + 10;
                    throw 5;
                    result = result + 100;
                } catch (e) {
                    result = result + e;
                    throw 20;
                } finally {
                    result = result + 1000;
                }
            } catch (e) {
                result = result + e;
            } finally {
                result = result + 10000;
            }
            return result;
        }
    "#;
    let compiled3 =
        compile_typescript_waffle(source3, "nested_try.ts", &WaffleCompileOptions::default())?;
    let component3 = Component::new(&engine, compiled3.component.unwrap())?;
    let mut store3 = Store::new(&engine, WasiHostState::default());
    let instance3 = linker.instantiate_async(&mut store3, &component3).await?;
    let run3 = instance3.get_typed_func::<(f64,), (f64,)>(&mut store3, "run")?;

    // input = 0:
    // outer try: result = 1
    // inner try: result = 11, throw 5
    // inner catch: result = 11 + 5 = 16, throw 20
    // inner finally: result = 16 + 1000 = 1016, rethrows 20
    // outer catch: result = 1016 + 20 = 1036
    // outer finally: result = 1036 + 10000 = 11036
    // return 11036
    let (res_nested,) = run3.call_async(&mut store3, (0.0,)).await?;
    assert_eq!(res_nested, 11036.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_multi_frame_unwinding() -> Result<()> {
    let source = r#"
        function stepC(val: number): number {
            throw val + 1;
        }
        function stepB(val: number): number {
            return stepC(val * 2);
        }
        function stepA(val: number): number {
            return stepB(val + 3);
        }
        export function run(input: number): number {
            try {
                return stepA(input);
            } catch (err) {
                return err * 10;
            }
        }
    "#;
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;

    let compiled =
        compile_typescript_waffle(source, "unwind.ts", &WaffleCompileOptions::default())?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // input = 5:
    // stepA(5) -> stepB(8) -> stepC(16) -> throw 17
    // caught in run -> err * 10 = 170
    let (res,) = run.call_async(&mut store, (5.0,)).await?;
    assert_eq!(res, 170.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_infallible_uncaught_throw_traps() -> Result<()> {
    let source = r#"
        export function run(input: number): number {
            if (input < 0) {
                throw 500;
            }
            return input * 2;
        }
    "#;
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;

    let compiled =
        compile_typescript_waffle(source, "uncaught.ts", &WaffleCompileOptions::default())?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // Normal invocation succeeds
    let (res,) = run.call_async(&mut store, (10.0,)).await?;
    assert_eq!(res, 20.0);

    // Uncaught exception MUST trap and NOT return dummy value (e.g. 0.0)
    let err = run.call_async(&mut store, (-1.0,)).await.unwrap_err();
    let err_str = format!("{err:?}");
    assert!(
        err_str.contains("unreachable") || err_str.contains("Trap"),
        "Expected host trap on uncaught throw, got: {err_str}"
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_wit_domain_errors_and_instance_reuse() -> Result<()> {
    let source = r#"
        export function run(input: number): Result<number, number> {
            if (input < 0) {
                throw 404;
            }
            return input * 2;
        }
    "#;
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;

    let compiled =
        compile_typescript_waffle(source, "domain_error.ts", &WaffleCompileOptions::default())?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;

    let run =
        instance.get_typed_func::<(f64,), (std::result::Result<f64, f64>,)>(&mut store, "run")?;

    // 1. Success returns Ok
    let (res_ok,) = run.call_async(&mut store, (21.0,)).await?;
    assert_eq!(res_ok, Ok(42.0));

    // 2. Exception mapped to declared WIT domain error returns Err without trapping host
    let (res_err,) = run.call_async(&mut store, (-5.0,)).await?;
    assert_eq!(res_err, Err(404.0));

    // 3. Repeated invocations on the SAME instance demonstrate full instance reusability
    for i in 1..=5 {
        let (res_loop_ok,) = run.call_async(&mut store, (i as f64,)).await?;
        assert_eq!(res_loop_ok, Ok((i * 2) as f64));

        let (res_loop_err,) = run.call_async(&mut store, (-(i as f64),)).await?;
        assert_eq!(res_loop_err, Err(404.0));
    }

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_await_immediate_and_internal_async() -> Result<()> {
    let source = r#"
        async function step(val: number): Promise<number> {
            return val + 10;
        }

        export async function run(input: number): Promise<number> {
            let immediate = await (input * 3);
            let from_call = await step(immediate);
            let literal = await 5;
            return from_call + literal;
        }
    "#;
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;

    let compiled = compile_typescript_waffle(
        source,
        "immediate_await.ts",
        &WaffleCompileOptions::default(),
    )?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // input = 4:
    // immediate = 4 * 3 = 12
    // from_call = step(12) = 22
    // literal = 5
    // return 22 + 5 = 27
    let (res,) = run.call_async(&mut store, (4.0,)).await?;
    assert_eq!(res, 27.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_await_rejection_enters_guest_exception_path() -> Result<()> {
    let source = r#"
        async function fallible(val: number): Promise<number> {
            if (val < 0) {
                throw 88;
            }
            return val * 2;
        }

        export async function run(input: number): Promise<number> {
            let tracker = 1000;
            let result = 0;
            try {
                tracker = tracker + 100;
                let val = await fallible(input);
                result = val;
            } catch (err) {
                result = err + 12;
            } finally {
                tracker = tracker + 5000;
            }
            return tracker + result;
        }
    "#;
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;

    let compiled =
        compile_typescript_waffle(source, "async_reject.ts", &WaffleCompileOptions::default())?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // 1. Success case: input = 5
    // tracker = 1000 + 100 = 1100
    // val = 10, result = 10
    // finally: tracker = 1100 + 5000 = 6100
    // return 6100 + 10 = 6110
    let (res_ok,) = run.call_async(&mut store, (5.0,)).await?;
    assert_eq!(res_ok, 6110.0);

    // 2. Rejection case: input = -1
    // tracker = 1000 + 100 = 1100
    // fallible throws 88 -> caught in catch
    // result = 88 + 12 = 100
    // finally: tracker = 1100 + 5000 = 6100
    // return 6100 + 100 = 6200
    let (res_err,) = run.call_async(&mut store, (-1.0,)).await?;
    assert_eq!(res_err, 6200.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_async_multiple_awaits_in_loop_and_branch() -> Result<()> {
    let source = r#"
        declare function hostDouble(value: number): Promise<number>;
        declare function waitFor(milliseconds: number): Promise<void>;

        export async function run(input: number): Promise<number> {
            let acc = input;
            let i = 0;
            while (i < 3) {
                if (i === 1) {
                    await waitFor(10);
                    acc = acc + 7;
                } else {
                    acc = await hostDouble(acc);
                }
                i = i + 1;
            }
            return acc;
        }
    "#;
    let engine = make_async_engine()?;
    let component = Component::new(
        &engine,
        compile_typescript_waffle(source, "multi_await.ts", &WaffleCompileOptions::default())?
            .component
            .unwrap(),
    )?;

    let mut linker = make_wasi_linker(&engine)?;
    linker
        .root()
        .func_wrap_concurrent("host-double", |_, (value,): (f64,)| {
            Box::pin(async move { Ok((value * 2.0,)) })
        })?;

    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // input = 3:
    // i = 0: else branch -> hostDouble(3) = 6
    // i = 1: if branch -> waitFor(10), acc = 6 + 7 = 13
    // i = 2: else branch -> hostDouble(13) = 26
    // return 26
    let (res,) = run.call_async(&mut store, (3.0,)).await?;
    assert_eq!(res, 26.0);

    // Repeated calls on same instance
    for n in 1..=3 {
        let (r,) = run.call_async(&mut store, (n as f64,)).await?;
        // n -> 2n -> 2n + 7 -> (2n + 7) * 2 = 4n + 14
        assert_eq!(r, (4 * n + 14) as f64);
    }

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_async_cancellation_and_repeated_calls() -> Result<()> {
    let source = r#"
        declare function waitFor(milliseconds: number): Promise<void>;

        export async function run(input: number): Promise<number> {
            let val = input;
            // Sleep for 200ms
            await waitFor(200);
            return val * 10;
        }
    "#;
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;

    let compiled =
        compile_typescript_waffle(source, "cancel_task.ts", &WaffleCompileOptions::default())?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    // 1. First invocation is cancelled early via timeout
    {
        let invocation = run.call_async(&mut store, (5.0,));
        // Drop after 15ms (task is waiting 200ms)
        let cancel_res = timeout(Duration::from_millis(15), invocation).await;
        assert!(cancel_res.is_err(), "Expected timeout cancellation");
    }

    // 2. Subsequent invocation runs to completion without stale state poisoning
    {
        let invocation = run.call_async(&mut store, (7.0,));
        let (res,) = timeout(Duration::from_secs(2), invocation).await??;
        assert_eq!(res, 70.0);
    }

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_numeric_truthiness() -> Result<()> {
    let source = r#"
        export function run(input: number): number {
            let result = 0;
            if (input) { result = result + 1; }
            while (input) { return result + 2; }
            if (0 / 0) { return 99; }
            return result;
        }
    "#;
    let compiled =
        compile_typescript_waffle(source, "truthiness.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    for (input, expected) in [
        (0.0, 0.0),
        (-0.0, 0.0),
        (f64::NAN, 0.0),
        (1.0, 3.0),
        (-1.0, 3.0),
        (f64::INFINITY, 3.0),
        (f64::NEG_INFINITY, 3.0),
    ] {
        assert_eq!(
            run.call_async(&mut store, (input,)).await?,
            (expected,),
            "input: {input}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_exception_clause_local_scopes() -> Result<()> {
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;
    for clauses in [
        "try { let temporary = 3; if (input < 0) throw input; result = temporary; } catch (error) { let caught = error; result = caught; }",
        "try { let temporary = 3; if (input < 0) throw input; result = temporary; } catch (error) { let caught = await error; result = caught; } finally { let cleanup = await 10; result = result + cleanup; }",
        "try { try { let temporary = 3; if (input < 0) throw input; result = temporary; } finally { let cleanup = await 10; result = result + cleanup; } } catch (error) { let caught = await error; result = result + caught; }",
    ] {
        let source = format!(
            r#"
            export async function run(input: number): Promise<number> {{
                let result = 0;
                {clauses}
                await 1;
                if (input > 0) {{ let increment = 1; result = result + increment; }}
                let index = 0;
                while (index < 2) {{ result = result + await 1; index = index + 1; }}
                return result;
            }}
        "#
        );
        let compiled = compile_typescript_waffle(
            &source,
            "clause_scopes.ts",
            &WaffleCompileOptions::default(),
        )?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, WasiHostState::default());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
        let cleanup = if clauses.contains("finally") {
            10.0
        } else {
            0.0
        };
        for (input, expected) in [(-2.0, cleanup), (2.0, cleanup + 6.0)] {
            assert_eq!(
                run.call_async(&mut store, (input,)).await?,
                (expected,),
                "{clauses}"
            );
        }
    }
    Ok(())
}

#[test]
fn test_waffle_rejects_nonnumeric_thrown_payloads() {
    for source in [
        "export function run(input: number): number { try { throw true; } catch (e) { if (e === 1) return 10; return 20; } }",
        "export function run(input: number): number { try { throw false; } finally { let cleanup = 1; } }",
        "function fail(flag: boolean): number { throw flag; } export function run(input: number): number { return fail(true); }",
        "async function fail(): Promise<boolean> { return true; } export async function run(input: number): Promise<number> { throw await fail(); }",
    ] {
        for componentize in [false, true] {
            let options = WaffleCompileOptions {
                componentize,
                ..Default::default()
            };
            let error = compile_typescript_waffle(source, "throw_type.ts", &options).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Only numeric thrown payloads are supported"),
                "{error:#}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_wit_boolean_success_payloads() -> Result<()> {
    let source = r#"
        export function run(input: number): Result<boolean, number> {
            try {
                if (input < 0) { throw 42; }
                if (input > 0) { return true; }
                return false;
            } finally { let cleanup = 1; }
        }
    "#;
    let compiled =
        compile_typescript_waffle(source, "bool_result.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run =
        instance.get_typed_func::<(f64,), (std::result::Result<bool, f64>,)>(&mut store, "run")?;
    for (input, expected) in [
        (1.0, Ok(true)),
        (-1.0, Err(42.0)),
        (0.0, Ok(false)),
        (1.0, Ok(true)),
    ] {
        assert_eq!(run.call_async(&mut store, (input,)).await?, (expected,));
    }
    for ok_type in ["number", "boolean"] {
        let source = format!(
            "export function run(input: number): Result<{ok_type}, boolean> {{ return input; }}"
        );
        for componentize in [false, true] {
            let options = WaffleCompileOptions {
                componentize,
                ..Default::default()
            };
            let error = compile_typescript_waffle(&source, "bool_error.ts", &options).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("WIT Result error payloads must be numeric"),
                "{error:#}"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_exported_guest_calls_preserve_exception_semantics() -> Result<()> {
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;
    for return_type in [
        "number",
        "Result<number, number>",
        "Promise<number>",
        "Promise<Result<number, number>>",
    ] {
        let (async_modifier, await_modifier) = if return_type.starts_with("Promise") {
            ("async", "await")
        } else {
            ("", "")
        };
        let source = format!(
            r#"
            export async function run(input: number): Promise<number> {{
                let result = 0;
                try {{ result = await helper(input); }}
                catch (error) {{ result = error * 10; }}
                finally {{ result = result + await 1000; }}
                return result;
            }}
            export {async_modifier} function helper(input: number): {return_type} {{
                try {{
                    if (input < 0) {{ throw 7; }}
                    if (input > 3) {{ return {await_modifier} helper(input - 1) + 1; }}
                    return input * 2;
                }} finally {{ if (input === -2) {{ throw 8; }} }}
            }}
        "#
        );
        let compiled = compile_typescript_waffle(
            &source,
            "export_calls.ts",
            &WaffleCompileOptions::default(),
        )?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, WasiHostState::default());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
        for (input, expected) in [(2.0, 1004.0), (-1.0, 1070.0), (-2.0, 1080.0), (5.0, 1008.0)] {
            assert_eq!(
                run.call_async(&mut store, (input,)).await?,
                (expected,),
                "{return_type}"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_waffle_exported_boolean_and_void_guest_calls() -> Result<()> {
    let source = r#"
        export async function run(input: number): Promise<number> {
            let result = 0;
            try {
                let answer = await helper(input);
                if (answer) { result = 1; }
                await done(input);
            } catch (error) { result = error * 10; }
            finally { result = result + 1000; }
            return result;
        }
        export async function helper(input: number): Promise<Result<boolean, number>> {
            if (input < 0) { throw 7; }
            return true;
        }
        export async function done(input: number): Promise<void> {
            if (input === 0) { throw 8; }
            return;
        }
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "export_primitives.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    for (input, expected) in [(1.0, 1001.0), (-1.0, 1070.0), (0.0, 1080.0)] {
        assert_eq!(run.call_async(&mut store, (input,)).await?, (expected,));
    }
    Ok(())
}
