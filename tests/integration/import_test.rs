//! Application imports use the same checked canonical values as component exports.
use anyhow::Result;
use perry_wit::compiler::compile_for_world_with_memory_checks;
use wasmtime::{
    Engine, Store,
    component::{Component, ComponentType, Lift, Linker, Lower},
};

use crate::engine::async_engine;

#[derive(ComponentType, Lift, Lower, Debug, PartialEq)]
#[component(record)]
struct Packet {
    label: String,
    bytes: Vec<u8>,
    values: Vec<f64>,
}

#[derive(ComponentType, Lift, Debug, PartialEq)]
#[component(record)]
struct CompletionError {
    name: String,
    message: String,
}

#[test]
fn module_binding_guards_preserve_initialization_and_write_effects() -> Result<()> {
    let component = crate::world::component(
        "package test:initialization; world app { export run: func() -> result<f64>; }",
        "app",
        "initialization.ts",
        r#"
        let effects = 0;
        const step = 1;
        class Step { amount = step; }
        const failures = probe();
        let cell: number | undefined = undefined;
        function value(): number { effects += new Step().amount; return 3; }
        function probe(): number {
            let failures = 0;
            try { const observed = cell; } catch { failures++; }
            try { cell = value(); } catch { failures++; }
            return failures;
        }
        export function run(): number {
            const absent = cell === undefined;
            return (cell = 5, failures * 100 + effects * 10 + (absent ? 1 : 0) + cell);
        }
        "#,
    )?;
    let (mut store, instance) = crate::world::instantiate(&Engine::default(), &component)?;
    let run = instance.get_typed_func::<(), (std::result::Result<f64, ()>,)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, ())?.0.unwrap(), 216.0);
    assert_eq!(run.call(&mut store, ())?.0.unwrap(), 215.0);
    Ok(())
}

#[test]
fn typed_imports_preserve_results_bytes_and_module_initialization() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "imports.wit",
        r#"
        package test:imports;
        interface service {
            record packet { label: string, bytes: list<u8>, values: list<f64> }
            initialize: func() -> u32;
            exchange: func(input: packet) -> result<packet, string>;
            wide: func(value: s64) -> s64;
            observe: func(value: string);
        }
        world app { import service; export run: func(n: u32) -> result<string>; }
    "#,
    )?;
    let world = resolve.select_world(&[package], Some("app"))?;
    let compiled = compile_for_world_with_memory_checks(
        r#"
        import { initialize, exchange, wide, observe } from 'test:imports/service';
        const initial = initialize();
        export function run(n: number): string {
            let text = '';
            for (let i = 0; i < n; i++) {
                const result = exchange({ label: 'hé😀', bytes: 'hé😀', values: [initial, i] });
                if (result.ok) {
                    const echoed = exchange(result.value);
                    if (echoed.ok) text = echoed.value.label + ':' + echoed.value.bytes.length;
                }
                else { text = result.error; }
            }
            observe(text);
            return text + ':' + wide(9007199254740993n);
        }
    "#,
        "imports.ts",
        &resolve,
        world,
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
    let mut linker = Linker::<(u32, String)>::new(&engine);
    let mut host = linker.instance("test:imports/service")?;
    host.func_wrap("initialize", |mut store, (): ()| {
        store.data_mut().0 += 1;
        Ok((17u32,))
    })?;
    host.func_wrap("exchange", |_, (input,): (Packet,)| {
        assert_eq!(input.bytes, input.label.as_bytes());
        assert_eq!(input.values[0], 17.0);
        let result: Result<Packet, String> = if input.values[1] == 1.0 {
            Err("failure".into())
        } else {
            Ok(input)
        };
        Ok((result,))
    })?;
    host.func_wrap("wide", |_, (value,): (i64,)| Ok((-value,)))?;
    host.func_wrap("observe", |mut store, (value,): (String,)| {
        store.data_mut().1 = value;
        Ok(())
    })?;
    let mut store = Store::new(&engine, (0, String::new()));
    let instance = linker.instantiate(&mut store, &component)?;
    let run =
        instance.get_typed_func::<(u32,), (std::result::Result<String, ()>,)>(&mut store, "run")?;
    for (n, expected) in [(2, "failure"), (1000, "hé😀:7")] {
        assert_eq!(
            run.call(&mut store, (n,))?.0.unwrap(),
            format!("{expected}:-9007199254740993")
        );
        assert_eq!(store.data().1, expected);
        assert_eq!(store.data().0, 1);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn asynchronous_imports_overlap_and_can_be_observed_repeatedly() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "async-imports.wit",
        r#"
        package test:imports;
        interface service {
            record packet { label: string, bytes: list<u8>, values: list<f64> }
            exchange: async func(input: packet) -> result<packet, string>;
            observe: async func(value: string);
        }
        world app { import service; export run: async func(n: u32) -> result<string>; }
    "#,
    )?;
    let world = resolve.select_world(&[package], Some("app"))?;
    let compiled = compile_for_world_with_memory_checks(
        r#"
        import { exchange, observe } from 'test:imports/service';
        export async function run(n: number): Promise<string> {
            let text = '';
            for (let i = 0; i < n; i++) {
                const left = exchange({ label: 'left', bytes: 'hé😀', values: [i, 1] });
                const right = exchange({ label: 'right', bytes: new Uint8Array([1,2,3]), values: [i, 2] });
                const a = await left;
                const again = await left;
                const b = await right;
                if (a.ok && b.ok && again.ok) {
                    const nativeLeft = exchange(a.value);
                    const nativeRight = exchange(b.value);
                    const c = await nativeLeft;
                    const d = await nativeRight;
                    if (c.ok && d.ok) text = c.value.label + d.value.label + again.value.bytes.length;
                    else throw new Error('native input failure');
                } else throw new Error('unexpected failure');
            }
            await observe(text);
            return text;
        }
    "#,
        "async-imports.ts",
        &resolve,
        world,
    )?;
    let engine = async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
    struct State {
        text: String,
        limits: wasmtime::StoreLimits,
    }
    let mut linker = Linker::<State>::new(&engine);
    let started = Arc::new(AtomicUsize::new(0));
    let calls = started.clone();
    let mut host = linker.instance("test:imports/service")?;
    host.func_wrap_concurrent("exchange", move |_, (input,): (Packet,)| {
        let calls = calls.clone();
        let pair = calls.fetch_add(1, Ordering::SeqCst) / 2;
        Box::pin(async move {
            while calls.load(Ordering::SeqCst) < pair * 2 + 2 {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
            let value: Result<Packet, String> = Ok(input);
            Ok((value,))
        })
    })?;
    host.func_wrap_concurrent("observe", |store, (value,): (String,)| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            store.with(|mut access| access.data_mut().text = value);
            Ok(())
        })
    })?;
    let mut store = Store::new(
        &engine,
        State {
            text: String::new(),
            limits: wasmtime::StoreLimitsBuilder::new()
                .memory_size(512 * 1024)
                .build(),
        },
    );
    store.limiter(|state| &mut state.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run =
        instance.get_typed_func::<(u32,), (std::result::Result<String, ()>,)>(&mut store, "run")?;
    let (result,) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run.call_async(&mut store, (1000,)),
    )
    .await??;
    let result = result.expect("fixture has no escaping failure");
    assert_eq!(result, "leftright7");
    assert_eq!(store.data().text, result);
    assert_eq!(started.load(Ordering::SeqCst), 4000);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn detached_imports_acknowledge_cancellation_before_task_cleanup() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Completion(Arc<AtomicUsize>);
    impl Drop for Completion {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "cancel-imports.wit",
        r#"
        package test:cancel;
        interface service { work: async func(value: list<string>) -> list<string>; }
        world app {
            import service;
            record failure { name: string, message: string }
            export run: async func(fail: bool) -> result<string, failure>;
        }
    "#,
    )?;
    let world = resolve.select_world(&[package], Some("app"))?;
    let compiled = compile_for_world_with_memory_checks(
        r#"
        import { work } from 'test:cancel/service';
        export async function run(fail: boolean): Promise<string> {
            const pending = work(['input', 'hé😀']);
            if (fail) throw new RangeError('original failure');
            return 'unfinished';
        }
    "#,
        "cancel-imports.ts",
        &resolve,
        world,
    )?;
    let engine = async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
    for immediate in [false, true] {
        let mut linker = Linker::<()>::new(&engine);
        let dropped = Arc::new(AtomicUsize::new(0));
        let calls = dropped.clone();
        linker
            .instance("test:cancel/service")?
            .func_wrap_concurrent("work", move |_, (input,): (Vec<String>,)| {
                assert_eq!(input, ["input", "hé😀"]);
                let completion = Completion(calls.clone());
                Box::pin(async move {
                    let _completion = completion;
                    if !immediate {
                        std::future::pending::<()>().await;
                    }
                    Ok((vec!["output".to_owned(), "😀".to_owned()],))
                })
            })?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance
            .get_typed_func::<(bool,), (std::result::Result<String, CompletionError>,)>(
                &mut store, "run",
            )?;
        for (calls, fail) in [false, true, false].into_iter().enumerate() {
            let (result,) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                run.call_async(&mut store, (fail,)),
            )
            .await??;
            let expected = if fail {
                Err(CompletionError {
                    name: "RangeError".into(),
                    message: "original failure".into(),
                })
            } else if immediate {
                Ok("unfinished".into())
            } else {
                Err(CompletionError {
                    name: "Error".into(),
                    message:
                        "Unresolved detached work at task boundary; pending operations cancelled"
                            .into(),
                })
            };
            assert_eq!(result, expected);
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                calls + 1,
                "host future must be dropped exactly once before the task completes"
            );
        }
    }
    Ok(())
}
