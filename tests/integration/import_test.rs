//! Application imports use the same checked canonical values as component exports.
use crate::world::Compiler::MemoryChecked;
use anyhow::Result;
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
fn typed_imports_preserve_results_bytes_and_module_initialization() -> Result<()> {
    let bytes = MemoryChecked.component(
        r#"
        package test:imports;
        interface service {
            record packet { label: string, bytes: list<u8>, values: list<f64> }
            initialize: func() -> u32;
            exchange: func(input: packet) -> result<packet, string>;
            wide: func(value: s64) -> s64;
            observe: func(value: string);
        }
        world app {
            import service;
            export guards: func() -> result<f64>;
            export run: func(n: u32) -> result<string>;
        }
    "#,
        "app",
        "imports.ts",
        r#"
        import { initialize, exchange, wide, observe } from 'test:imports/service';
        const initial = initialize();
        let effects = 0;
        const step = 1;
        class Step { amount = step; }
        const failures = probe();
        let cell: number | undefined = undefined;
        function value() { effects += new Step().amount; return 3; }
        function probe() {
            let failures = 0;
            try { const observed = cell; } catch { failures++; }
            try { cell = value(); } catch { failures++; }
            return failures;
        }
        export function guards(): number {
            const absent = cell === undefined;
            return (cell = 5, failures * 100 + effects * 10 + (absent ? 1 : 0) + cell);
        }
        export function run(n: number) {
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
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, bytes)?;
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
    let guards =
        instance.get_typed_func::<(), (std::result::Result<f64, ()>,)>(&mut store, "guards")?;
    assert_eq!(guards.call(&mut store, ())?.0.unwrap(), 216.0);
    assert_eq!(guards.call(&mut store, ())?.0.unwrap(), 215.0);
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
    let bytes = MemoryChecked.component(
        r#"
        package test:imports;
        interface service {
            record packet { label: string, bytes: list<u8>, values: list<f64> }
            exchange: async func(input: packet) -> result<packet, string>;
            observe: async func(value: string);
        }
        world app { import service; export run: async func(n: u32) -> result<string>; }
    "#,
        "app",
        "async-imports.ts",
        r#"
        import { exchange, observe } from 'test:imports/service';
        export async function run(n: number) {
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
    )?;
    let engine = async_engine()?;
    let component = Component::new(&engine, bytes)?;
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
    let bytes = MemoryChecked.component(
        r#"
        package test:cancel;
        interface service { work: async func(value: list<string>) -> list<string>; }
        world app {
            import service;
            record failure { name: string, message: string }
            export run: async func(fail: bool) -> result<string, failure>;
        }
    "#,
        "app",
        "cancel-imports.ts",
        r#"
        import { work } from 'test:cancel/service';
        export async function run(fail: boolean) {
            const pending = work(['input', 'hé😀']);
            if (fail) throw new RangeError('original failure');
            return 'unfinished';
        }
    "#,
    )?;
    let engine = async_engine()?;
    let component = Component::new(&engine, bytes)?;
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

#[test]
fn wit_flags_pack_partial_fields_and_lift_complete_objects() -> Result<()> {
    use wasmtime::component::Val;

    for width in [1, 8, 9, 16, 17, 32] {
        let mut names = vec!["first-bit".to_owned()];
        names.extend((1..width).map(|i| format!("f{i}")));
        let last = names.last().unwrap();
        let field = if width == 1 {
            "firstBit"
        } else {
            last.as_str()
        };
        let wit = format!("package test:bitset;
            interface service {{ flags bits {{ {} }} echo: func(value:bits) -> bits; }}
            world app {{ import service; use service.{{bits}};
                export relay:func(value:bits)->bits; export partial:func(on:bool)->bits; export empty:func(on:bool)->bits; export bound:func(on:bool)->bits;
                export rows:func(values:list<bits>)->list<bits>; export unset:func(value:bits)->bool; }}",
            names.join(", "));
        let source = format!("import {{echo}} from 'test:bitset/service';
            import type {{Bits}} from './types/world';
            export function relay(value:Bits):Bits {{ return echo(value); }}
            export function partial(on:boolean):Bits {{ return echo({{ {field}:on }}); }}
            export function bound(on:boolean):Bits {{ const opts = {{ {field}:on, label:'saved' }}; return echo(opts); }}
            export function empty(on:boolean):Bits {{ if(on) return echo({{}}); return echo({{ {field}:undefined }}); }}
            export function rows(values:Bits[]):Bits[] {{ return values; }}
            export function unset(value:Bits):boolean {{ return value.firstBit === false; }}");
        let bytes = MemoryChecked.component(&wit, "app", "flags.ts", &source)?;
        let engine = Engine::default();
        let component = Component::new(&engine, bytes)?;
        let mut linker = Linker::new(&engine);
        linker.instance("test:bitset/service")?.func_new(
            "echo",
            |_store, _ty, args, results| {
                results[0] = args[0].clone();
                Ok(())
            },
        )?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate(&mut store, &component)?;
        let mut check = |name: &str, arg: Val, expected: Val| -> Result<()> {
            let f = instance.get_func(&mut store, name).unwrap();
            let mut results = [Val::Bool(false)];
            f.call(&mut store, &[arg], &mut results)?;
            assert_eq!(results[0], expected, "{width} flags, {name}");
            Ok(())
        };
        let high = Val::Flags(vec![last.clone()]);
        let none = Val::Flags(Vec::new());
        check("relay", high.clone(), high.clone())?;
        check("partial", Val::Bool(true), high.clone())?;
        check("partial", Val::Bool(false), none.clone())?;
        check("bound", Val::Bool(true), high.clone())?;
        check("bound", Val::Bool(false), none.clone())?;
        check("empty", Val::Bool(true), none.clone())?;
        check("empty", Val::Bool(false), none.clone())?;
        check("unset", none.clone(), Val::Bool(true))?;
        let rows = Val::List(vec![high, none, Val::Flags(names)]);
        check("rows", rows.clone(), rows)?;
    }
    Ok(())
}
