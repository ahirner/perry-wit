//! Native subtask protocol probes and stored Promise integration tests.

#[path = "support/waffle.rs"]
mod waffle_fixture;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use waffle_fixture::compile_typescript_waffle;

use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use tokio::sync::Notify;
use tokio::time::timeout;
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

fn make_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_threading(true);
    config.wasm_component_model_more_async_builtins(true);
    Ok(Engine::new(&config)?)
}

#[tokio::test(flavor = "current_thread")]
async fn captured_arguments_preserve_distinct_calls_and_instance_reuse() -> Result<()> {
    let source = r#"
        async function choose(flag:boolean):Promise<number> {
            await 0;
            return flag ? 11 : 22;
        }
        async function constant(flag:boolean):Promise<number> {
            await 0;
            return flag ? 1000 : 100;
        }
        export async function run(input:number):Promise<number> {
            const first = choose(false);
            const second = choose(input === 1);
            const third = constant(false);
            return (await first) + (await second) + (await third);
        }
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "captured_arguments.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    for input in [1.0, 0.0, 1.0, -1.0] {
        assert_eq!(
            run.call_async(&mut store, (input,)).await?.0,
            if input == 1.0 { 133.0 } else { 144.0 }
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn core_export_results_survive_ready_task_collection() -> Result<()> {
    for (result_type, expression, expected) in [
        (
            "string",
            "input + ' retained result'",
            Val::String("A.B.C retained result".into()),
        ),
        (
            "Result<string, number>",
            "input + ' retained result'",
            Val::Result(Ok(Some(Box::new(Val::String(
                "A.B.C retained result".into(),
            ))))),
        ),
        (
            "string[]",
            "(input + ' first,' + input + ' second').split(',')",
            Val::List(vec![
                Val::String("A.B.C first".into()),
                Val::String("A.B.C second".into()),
            ]),
        ),
    ] {
        let source = format!(
            "async function cleanup(input:string):Promise<void> {{
                await 0;
                for(let i=0;i<2000;i++) {{
                    const discarded=input.toLowerCase().split('.').join('-');
                }}
            }}
            export function run(input:string):{result_type} {{
                const pending=cleanup(input);
                return {expression};
            }}"
        );
        let compiled = compile_typescript_waffle(
            &source,
            "core_export_roots.ts",
            &WaffleCompileOptions::default(),
        )?;
        let engine = make_engine()?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(524_288).build(),
        );
        store.limiter(|limits| limits);
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let run = instance.get_func(&mut store, "run").unwrap();
        for _ in 0..50 {
            let mut result = [Val::Bool(false)];
            run.call_async(&mut store, &[Val::String("A.B.C".into())], &mut result)
                .await?;
            assert_eq!(result[0], expected, "{result_type}");
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[derive(Default)]
struct Host {
    context: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

#[derive(Default)]
struct OperationCounts {
    active: AtomicUsize,
    started: AtomicUsize,
    completed: AtomicUsize,
    samples: AtomicUsize,
}

struct ActiveOperation(Arc<OperationCounts>);

impl Drop for ActiveOperation {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn gated_linker(
    engine: &Engine,
    gate: Arc<Notify>,
    counts: Arc<OperationCounts>,
) -> Result<Linker<StoreLimits>> {
    let mut linker = Linker::new(engine);
    let clock_counts = counts.clone();
    linker
        .instance("wasi:clocks/monotonic-clock@0.3.0")?
        .func_wrap_concurrent("wait-for", move |_, (_duration,): (u64,)| {
            let gate = gate.clone();
            let counts = clock_counts.clone();
            counts.started.fetch_add(1, Ordering::SeqCst);
            counts.active.fetch_add(1, Ordering::SeqCst);
            let operation = ActiveOperation(counts.clone());
            Box::pin(async move {
                let _operation = operation;
                gate.notified().await;
                counts.completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })?;
    linker.instance("wasi:random/random@0.3.0")?.func_wrap(
        "get-random-u64",
        move |_: StoreContextMut<'_, StoreLimits>, (): ()| {
            counts.samples.fetch_add(1, Ordering::SeqCst);
            Ok((0u64,))
        },
    )?;
    Ok(linker)
}

#[tokio::test(flavor = "current_thread")]
async fn long_invocations_reclaim_dead_promises_and_retain_shared_outcomes() -> Result<()> {
    let source = r#"async function produce(input: string): Promise<string> { return input.toUpperCase(); }
        export async function run(input: string, count: number): Promise<string> {
            const retained = produce(input);
            const alias = retained;
            await retained;
            for (let i = 0; i < count; i++) {
                const temporary = produce(input);
                await temporary;
            }
            if (alias !== retained) throw 11;
            return await alias;
        }"#;
    let compiled = compile_typescript_waffle(
        source,
        "promise_lifetimes.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = Linker::new(&engine);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(524_288).build(),
    );
    store.limiter(|limits| limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str, f64), (String,)>(&mut store, "run")?;
    let input = "ß🦀".repeat(512);
    for count in [1.0, 2_000.0] {
        assert_eq!(
            run.call_async(&mut store, (&input, count)).await?.0,
            "SS🦀".repeat(512)
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn collection_retains_suspended_tasks_observers_and_finally_returns() -> Result<()> {
    let source = r#"
        import { setTimeout as waitFor } from "node:timers/promises";

        function churn(s: string): void {
            for (let i = 0; i < 2000; i++) {
                const temporary = s.toLowerCase().split(".").join("-");
            }
        }
        async function produce(s: string, fail: boolean): Promise<string> {
            const retained = s.toUpperCase().slice(1).split(".");
            await waitFor(1);
            if (fail) throw 7;
            return retained.join("|");
        }
        async function observe(shared: Promise<string>, s: string): Promise<string> {
            try { return (await shared).slice(0); }
            finally { churn(s); Math.random(); }
        }
        export async function run(s: string, fail: boolean): Promise<Result<string, number>> {
            const shared = produce(s, fail);
            const first = observe(shared, s);
            const second = observe(shared, s);
            churn(s);
            Math.random();
            let error = 0;
            let result = "";
            try { result = await first; } catch (e) { error = e as number; }
            try { result = result + await second; } catch (e) { error = e as number; }
            if (error !== 0) throw error;
            return result;
        }
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "suspended_roots.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let input = "ß🦀.".repeat(128);
    let expected = format!("S🦀|{}", "SS🦀|".repeat(127)).repeat(2);
    for (fail, dispose) in [(false, false), (true, false), (false, true), (true, true)] {
        let gate = Arc::new(Notify::new());
        let counts = Arc::new(OperationCounts::default());
        let linker = gated_linker(&engine, gate.clone(), counts.clone())?;
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(524_288).build(),
        );
        store.limiter(|limits| limits);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(&str, bool), (std::result::Result<String, f64>,)>(
            &mut store, "run",
        )?;
        let mut pending = Box::pin(run.call_async(&mut store, (&input, fail)));
        assert!(
            timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        assert_eq!(counts.started.load(Ordering::SeqCst), 1);
        assert_eq!(counts.active.load(Ordering::SeqCst), 1);
        assert_eq!(counts.samples.load(Ordering::SeqCst), 1);
        if dispose {
            drop(pending);
        } else {
            gate.notify_one();
            let result = timeout(Duration::from_secs(5), pending).await??.0;
            assert_eq!(result, if fail { Err(7.0) } else { Ok(expected.clone()) });
            assert_eq!(counts.samples.load(Ordering::SeqCst), 3);
            store.assert_concurrent_state_empty();
        }
        drop(store);
        assert_eq!(counts.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            counts.samples.load(Ordering::SeqCst),
            if dispose { 1 } else { 3 }
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn invocation_disposal_releases_parked_observers_and_ready_races() -> Result<()> {
    let source = r#"
        import { setTimeout as waitFor } from "node:timers/promises";

        async function produce(s: string, fail: boolean): Promise<string> {
            let retained = s.toUpperCase();
            await waitFor(1);
            if (fail) throw 7;
            return retained;
        }
        async function observe(shared: Promise<string>): Promise<string> {
            try { return await shared; } finally { Math.random(); }
        }
        export async function run(s: string, fail: boolean): Promise<Result<string, number>> {
            const shared = produce(s, fail);
            const first = observe(shared);
            const second = observe(shared);
            let error = 0;
            let result = "";
            try { result = await first; } catch (e) { error = e as number; }
            try { result = result + await second; } catch (e) { error = e as number; }
            if (error !== 0) throw error;
            return result;
        }
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "cancel_observers.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let input = "é🦀".repeat(4096);
    for cycle in 0..18 {
        let gate = Arc::new(Notify::new());
        let counts = Arc::new(OperationCounts::default());
        let linker = gated_linker(&engine, gate.clone(), counts.clone())?;
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(524_288).build(),
        );
        store.limiter(|limits| limits);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(&str, bool), (std::result::Result<String, f64>,)>(
            &mut store, "run",
        )?;
        let failed = cycle % 2 == 0;
        let mut pending = Box::pin(run.call_async(&mut store, (&input, failed)));
        assert!(
            timeout(Duration::from_millis(5), &mut pending)
                .await
                .is_err()
        );
        assert_eq!(counts.started.load(Ordering::SeqCst), 1);
        assert_eq!(counts.active.load(Ordering::SeqCst), 1);
        match cycle % 3 {
            0 => drop(pending),
            1 => {
                gate.notify_one();
                drop(pending);
            }
            _ => {
                gate.notify_one();
                let result = timeout(Duration::from_secs(2), pending).await??.0;
                assert_eq!(
                    result,
                    if failed {
                        Err(7.0)
                    } else {
                        Ok("É🦀".repeat(8192))
                    }
                );
                assert_eq!(counts.completed.load(Ordering::SeqCst), 1);
                assert_eq!(counts.samples.load(Ordering::SeqCst), 2);
                store.assert_concurrent_state_empty();
            }
        }
        drop(store);
        assert_eq!(counts.active.load(Ordering::SeqCst), 0);
        if cycle % 3 != 2 {
            assert_eq!(counts.completed.load(Ordering::SeqCst), 0);
            assert_eq!(counts.samples.load(Ordering::SeqCst), 0);
        }
        gate.notify_waiters();
        tokio::task::yield_now().await;
        assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn abandoned_pending_work_never_becomes_a_successful_result() -> Result<()> {
    let engine = make_engine()?;
    for exit in ["return 1;", "throw 8;"] {
        let source = format!(
            r#"import {{ setTimeout as waitFor }} from "node:timers/promises";
            export async function run(): Promise<Result<number, number>> {{ const pending = waitFor(1); {exit} }}"#
        );
        let compiled =
            compile_typescript_waffle(&source, "abandoned.ts", &WaffleCompileOptions::default())?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let gate = Arc::new(Notify::new());
        let counts = Arc::new(OperationCounts::default());
        let linker = gated_linker(&engine, gate, counts.clone())?;
        let mut store = Store::new(&engine, StoreLimits::default());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run =
            instance.get_typed_func::<(), (std::result::Result<f64, f64>,)>(&mut store, "run")?;
        let result = timeout(Duration::from_secs(2), run.call_async(&mut store, ())).await?;
        assert!(result.is_err(), "{exit}: unobserved work must trap");
        drop(store);
        assert_eq!(counts.started.load(Ordering::SeqCst), 1);
        assert_eq!(counts.completed.load(Ordering::SeqCst), 0);
        assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn one_observers_failure_does_not_cancel_the_shared_operation() -> Result<()> {
    let source = r#"
        import { setTimeout as waitFor } from "node:timers/promises";

        async function produce(): Promise<string> { await waitFor(1); return "A🦀"; }
        async function observe(shared: Promise<string>, fail: boolean): Promise<string> {
            if (fail) throw 3;
            return await shared;
        }
        export async function run(): Promise<number> {
            const shared = produce();
            const first = observe(shared, true);
            const second = observe(shared, false);
            let error = 0;
            try { await first; } catch (e) { error = e as number; }
            Math.random();
            const value = await second;
            return error + value.length;
        }
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "observer_failure.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let gate = Arc::new(Notify::new());
    let counts = Arc::new(OperationCounts::default());
    let linker = gated_linker(&engine, gate.clone(), counts.clone())?;
    let mut store = Store::new(&engine, StoreLimits::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    let mut pending = Box::pin(run.call_async(&mut store, ()));
    assert!(
        timeout(Duration::from_millis(10), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(counts.active.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.samples.load(Ordering::SeqCst),
        1,
        "the first observer's rejection was already caught"
    );
    gate.notify_one();
    assert_eq!(timeout(Duration::from_secs(2), pending).await??.0, 5.0);
    assert_eq!(counts.completed.load(Ordering::SeqCst), 1);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn component_encoder_serializes_overlapping_entry_calls() -> Result<()> {
    let source = r#"import { setTimeout as waitFor } from "node:timers/promises";
        export async function run(): Promise<number> { const pending = waitFor(1); await pending; return 1; }"#;
    let compiled =
        compile_typescript_waffle(source, "overlapping.ts", &WaffleCompileOptions::default())?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let counts = Arc::new(OperationCounts::default());
    let gate = Arc::new(Notify::new());
    let linker = gated_linker(&engine, gate.clone(), counts.clone())?;
    let mut store = Store::new(&engine, StoreLimits::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    timeout(
        Duration::from_secs(2),
        store.run_concurrent(async |accessor| {
            let mut first = Box::pin(run.call_concurrent(accessor, ()));
            assert!(
                timeout(Duration::from_millis(10), &mut first)
                    .await
                    .is_err()
            );
            let mut second = Box::pin(run.call_concurrent(accessor, ()));
            assert!(
                timeout(Duration::from_millis(10), &mut second)
                    .await
                    .is_err()
            );
            assert_eq!(counts.started.load(Ordering::SeqCst), 1);
            gate.notify_one();
            assert_eq!(first.await?, (1.0,));
            gate.notify_one();
            assert_eq!(second.await?, (1.0,));
            Ok::<_, anyhow::Error>(())
        }),
    )
    .await???;
    assert_eq!(counts.started.load(Ordering::SeqCst), 2);
    store.assert_concurrent_state_empty();
    drop(store);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn completed_task_transport_releases_native_state() -> Result<()> {
    let engine = make_engine()?;
    let linker = Linker::new(&engine);
    for (body, observation) in [
        ("return 7;", "return await pending;"),
        ("throw 7;", "return await pending;"),
        ("await 0; return 7;", "return await pending;"),
        ("return 7;", "return 7;"),
    ] {
        let source = format!(
            "async function done(): Promise<number> {{ {body} }} export async function run(): Promise<number> {{ const pending = done(); try {{ {observation} }} catch (e) {{ return e; }} }}"
        );
        let compiled = compile_typescript_waffle(
            &source,
            "completed_transport.ts",
            &WaffleCompileOptions::default(),
        )?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        for iteration in 0..4 {
            assert_eq!(run.call_async(&mut store, ()).await?.0, 7.0);
            assert_eq!(
                store.concurrent_state_table_size(),
                0,
                "{body}, invocation {iteration}"
            );
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn settled_promise_identity_adoption_and_void_are_preserved() -> Result<()> {
    let engine = make_engine()?;
    let linker = Linker::new(&engine);
    for (source, expected) in [
        (
            r#"async function done(): Promise<boolean> { return true; }
            export async function run(): Promise<number> {
                const first = done(); const second = done(); const alias = first;
                if (first !== alias) return 1;
                if (first === second) return 2;
                if (first === undefined) return 3;
                if (first === true) return 4;
                if (first === "") return 5;
                if (await first !== await second) return 6;
                return 7;
            }
            export function task_0(): number { return 99; }
            export function task_1(): number { return 99; }
            "#,
            7.0,
        ),
        (
            r#"async function done(): Promise<number> { return 42; }
            async function adopt(): Promise<number> { const pending = done(); return pending; }
            export async function run(): Promise<number> { const pending = adopt(); return await pending; }"#,
            42.0,
        ),
        (
            r#"async function done(): Promise<void> {}
            export async function run(): Promise<number> { const pending = done(); const value = await pending; await pending; if (value === undefined) return 1; return 0; }"#,
            1.0,
        ),
        (
            r#"async function fail(): Promise<number> { throw 13; }
            export async function run(): Promise<number> {
                const pending = fail(); let sum = 0;
                try { await pending; } catch (error) { sum = sum + error; }
                try { await pending; } catch (error) { sum = sum + error; }
                return sum;
            }"#,
            26.0,
        ),
    ] {
        let compiled =
            compile_typescript_waffle(source, "settled.ts", &WaffleCompileOptions::default())?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call_async(&mut store, ()).await?.0, expected);
    }
    Ok(())
}

#[test]
fn unsupported_promise_uses_have_source_diagnostics() {
    for (body, expected) in [
        ("done(); return 1;", "Detached async calls are unsupported"),
        (
            "const p = done(); return p.length;",
            "Promise properties are unsupported",
        ),
        (
            "const p = done(); p[0]; return 1;",
            "Promise indexing is unsupported",
        ),
        (
            "const p = done(); p.cancel(); return 1;",
            "Promise methods are unsupported",
        ),
        (
            "const p = done(); return consume(p);",
            "Stored Promise arguments must match",
        ),
        (
            "let p: any = done(); if (flag) p = true; await p; return 1;",
            "A stored Promise binding cannot change",
        ),
        (
            "const p = done(); if (p < p) return 1; return 0;",
            "Promise values support strict identity comparisons only",
        ),
    ] {
        let source = format!(
            "async function done(): Promise<number> {{ return 1; }} function consume(value: boolean): number {{ return 1; }} export async function run(flag: boolean): Promise<number> {{ {body} }}"
        );
        let error = compile_typescript_waffle(
            &source,
            "unsupported_promise.ts",
            &WaffleCompileOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{body}: {error:#}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_observers_receive_one_shared_outcome() -> Result<()> {
    let source = r#"
        import { setTimeout as waitFor } from "node:timers/promises";
        declare function hostDouble(value: number): Promise<number>;
        async function produce(mode: number): Promise<number> {
            if (mode === 0) await waitFor(1);
            if (mode === 4) await waitFor(1);
            if (mode === 1) await 1;
            if (mode === 2) throw 7;
            if (mode === 4) throw 7;
            return 42;
        }
        async function observe(shared: Promise<number>, id: number): Promise<number> {
            let outcome = 0;
            try { outcome = await shared; } catch (error) { outcome = error as number; }
            await hostDouble(id);
            return outcome;
        }
        export async function run(mode: number): Promise<number> {
            const shared = produce(mode);
            const first = observe(shared, 1);
            const second = observe(shared, 2);
            const third = observe(shared, 3);
            const a = await first;
            await hostDouble(9);
            const b = await second;
            const c = await third;
            return a + b + c;
        }
    "#;
    let compiled =
        compile_typescript_waffle(source, "observers.ts", &WaffleCompileOptions::default())?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let release = Arc::new(Notify::new());
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut linker = Linker::new(&engine);
    let host_release = release.clone();
    let host_trace = trace.clone();
    linker
        .instance("wasi:clocks/monotonic-clock@0.3.0")?
        .func_wrap_concurrent("wait-for", move |_, (duration,): (u64,)| {
            let release = host_release.clone();
            let trace = host_trace.clone();
            Box::pin(async move {
                trace.lock().unwrap().push(format!("wait:{duration}"));
                release.notified().await;
                Ok(())
            })
        })?;
    let host_trace = trace.clone();
    linker
        .root()
        .func_wrap_concurrent("host-double", move |_, (value,): (f64,)| {
            let trace = host_trace.clone();
            Box::pin(async move {
                trace.lock().unwrap().push(format!("observe:{value}"));
                Ok((value * 2.0,))
            })
        })?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65_536).build(),
    );
    store.limiter(|limits| limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    let mut observed = Vec::new();
    for mode in [0.0, 1.0, 2.0, 3.0, 4.0] {
        trace.lock().unwrap().clear();
        let mut pending = Box::pin(run.call_async(&mut store, (mode,)));
        if mode == 0.0 || mode == 4.0 {
            assert!(
                timeout(Duration::from_millis(10), &mut pending)
                    .await
                    .is_err()
            );
            assert_eq!(*trace.lock().unwrap(), ["wait:1000000"]);
            release.notify_one();
        }
        let result = timeout(Duration::from_secs(2), pending).await??.0;
        assert_eq!(
            result,
            if mode == 2.0 || mode == 4.0 {
                21.0
            } else {
                126.0
            }
        );
        observed.push((result, trace.lock().unwrap().clone()));
    }
    let scratch = tempfile::tempdir()?;
    let node_path = scratch.path().join("observers.mts");
    let node_source = format!(
        "{}\n(globalThis as any).hostDouble=async (value:number)=>{{trace.push(`observe:${{value}}`);return value*2;}};\n{source}",
        include_str!("support/node-task-observer.mjs")
    );
    std::fs::write(
        &node_path,
        format!(
            "{node_source}\nconst observed = []; for (const mode of [0,1,2,3,4]) {{ trace.length = 0; observed.push([await run(mode), [...trace]]); }} console.log(JSON.stringify(observed));"
        ),
    )?;
    let node = std::process::Command::new("node").arg(node_path).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    let normalize = |samples: Vec<(f64, Vec<String>)>| {
        samples
            .into_iter()
            .map(|(result, mut trace)| {
                let position = |event| trace.iter().position(|value| value == event).unwrap();
                assert!(position("observe:1") < position("observe:9"));
                trace.sort();
                (result, trace)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        normalize(serde_json::from_slice::<Vec<(f64, Vec<String>)>>(
            &node.stdout
        )?),
        normalize(observed)
    );
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sibling_and_dependent_observers_each_run_once() -> Result<()> {
    let source = r#"
        import { setTimeout as waitFor } from "node:timers/promises";
        interface Trace { text:string }
        async function produce(): Promise<number> { await waitFor(1); return 42; }
        async function observe(shared: Promise<number>, name: string, trace:Trace): Promise<number> {
            const value = await shared;
            trace.text=trace.text+name+';';
            return value;
        }
        export async function run(): Promise<string> {
            const trace:Trace={text:''};
            const shared = produce();
            const first = observe(shared, 'one', trace);
            const second = observe(shared, 'two', trace);
            const third = observe(shared, 'three', trace);
            const a = await first;
            trace.text=trace.text+'dependent;';
            const b=await second;
            const c=await third;
            if(a!==42||b!==42||c!==42)throw 99;
            return trace.text;
        }
    "#;
    for source in [
        source.to_string(),
        source.replace("export async function run()", "async function adopt(shared: Promise<number>): Promise<number> { return shared; } export async function run()")
            .replace("observe(shared, 'one', trace)", "observe(adopt(shared), 'one', trace)"),
    ] {
        let compiled = compile_typescript_waffle(
            &source, "reaction_dependencies.ts", &WaffleCompileOptions::default(),
        )?;
        let engine = make_engine()?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let release = Arc::new(Notify::new());
        let host_release = release.clone();
        let mut linker = Linker::new(&engine);
        linker.instance("wasi:clocks/monotonic-clock@0.3.0")?
            .func_wrap_concurrent("wait-for", move |_, (_duration,): (u64,)| {
                let release = host_release.clone();
                Box::pin(async move { release.notified().await; Ok(()) })
            })?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        let mut pending = Box::pin(run.call_async(&mut store, ()));
        assert!(timeout(Duration::from_millis(10), &mut pending).await.is_err());
        release.notify_one();
        let actual = timeout(Duration::from_secs(2), pending).await??.0;
        store.assert_concurrent_state_empty();
        let scratch = tempfile::tempdir()?;
        let path = scratch.path().join("dependencies.mts");
        let node_source = &source;
        std::fs::write(&path, format!("{node_source}\nconsole.log(await run());"))?;
        let node = std::process::Command::new("node").arg(path).output()?;
        assert!(node.status.success(), "{}", String::from_utf8_lossy(&node.stderr));
        let expected = String::from_utf8(node.stdout)?;
        for trace in [actual.as_str(), expected.trim()] {
            let mut events: Vec<_> = trace.split(';').filter(|event| !event.is_empty()).collect();
            let first = events.iter().position(|event| *event == "one").unwrap();
            let dependent = events.iter().position(|event| *event == "dependent").unwrap();
            assert!(first < dependent, "{trace}");
            events.sort();
            assert_eq!(events, ["dependent", "one", "three", "two"]);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stored_text_results_survive_repeated_observation_and_export_cleanup() -> Result<()> {
    let engine = make_engine()?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let input = "é🦀".repeat(8192);
    let expected = format!("{}!", "É🦀".repeat(8192));
    for (result_type, result_expr, expected_result) in [
        ("string", "first", Val::String(expected.clone())),
        ("number", "first.length", Val::Float64(16385.0)),
        ("boolean", "first === second", Val::Bool(true)),
        (
            "Result<string, number>",
            "first",
            Val::Result(Ok(Some(Box::new(Val::String(expected.clone()))))),
        ),
        (
            "Result<number, number>",
            "first.length",
            Val::Result(Ok(Some(Box::new(Val::Float64(16385.0))))),
        ),
        (
            "Result<boolean, number>",
            "first === second",
            Val::Result(Ok(Some(Box::new(Val::Bool(true))))),
        ),
    ] {
        let source = format!(
            r#"
            import {{ setTimeout as waitFor }} from "node:timers/promises";
            async function work(s: string): Promise<string> {{
                let retained = s.toUpperCase();
                await waitFor(0.001);
                return retained + "!";
            }}
            async function observe(shared: Promise<string>): Promise<string> {{ return await shared; }}
            export async function run(s: string, fail: boolean): Promise<{result_type}> {{
                const pending = work(s);
                const firstObserver = observe(pending);
                const secondObserver = observe(pending);
                const first = await firstObserver;
                const second = await secondObserver;
                if (first !== second) throw 99;
                if (fail) throw 7;
                return {result_expr};
            }}
        "#
        );
        let compiled = compile_typescript_waffle(
            &source,
            "retained_text.ts",
            &WaffleCompileOptions::default(),
        )?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(
            &engine,
            Host {
                limits: StoreLimitsBuilder::new().memory_size(524_288).build(),
                ..Default::default()
            },
        );
        store.limiter(|state| &mut state.limits);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_func(&mut store, "run").unwrap();
        for iteration in 0..40 {
            let fail = result_type.starts_with("Result") && iteration % 2 == 0;
            let mut results = [Val::Bool(false)];
            run.call_async(
                &mut store,
                &[Val::String(input.clone()), Val::Bool(fail)],
                &mut results,
            )
            .await?;
            let expected = if fail {
                Val::Result(Err(Some(Box::new(Val::Float64(7.0)))))
            } else {
                expected_result.clone()
            };
            assert_eq!(results[0], expected, "{result_type}, iteration {iteration}");
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_awaits_do_not_allocate_additional_records() -> Result<()> {
    let source = r#"
        async function done(): Promise<number> { return 3; }
        export async function run(count: number): Promise<number> {
            const value = done();
            let index = 0;
            let sum = 0;
            while (index < count) {
                sum = sum + await value;
                index = index + 1;
            }
            return sum;
        }
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "retained_allocations.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    assert_eq!(component.component_type().imports(&engine).count(), 0);
    let linker = Linker::new(&engine);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65_536).build(),
    );
    store.limiter(|limits| limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    for count in [1.0, 10000.0, 10000.0] {
        assert_eq!(run.call_async(&mut store, (count,)).await?.0, count * 3.0);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn source_stored_promise_retains_identity_success_and_rejection() -> Result<()> {
    let source = r#"
        import { setTimeout as waitFor } from "node:timers/promises";

        async function work(value: number): Promise<number> {
            Math.random();
            await waitFor(1);
            Math.random();
            if (value < 0) throw 7;
            return value * 2;
        }
        export async function run(value: number): Promise<number> {
            const pending = work(value);
            const alias = pending;
            if (pending !== alias) return -1;
            Math.random();
            let total = 0;
            try { total = await pending; } catch (error) { total = error as number; }
            try { total = total + await alias; } catch (error) { total = total + (error as number); }
            return total;
        }
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "stored_promise.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let release = Arc::new(Notify::new());
    let trace = Arc::new(Mutex::new(Vec::new()));
    let host_release = release.clone();
    let host_trace = trace.clone();
    let mut linker = Linker::new(&engine);
    linker
        .instance("wasi:clocks/monotonic-clock@0.3.0")?
        .func_wrap_concurrent("wait-for", move |_, (duration,): (u64,)| {
            let release = host_release.clone();
            let trace = host_trace.clone();
            Box::pin(async move {
                trace.lock().unwrap().push(format!("wait:{duration}"));
                release.notified().await;
                Ok(())
            })
        })?;
    let host_trace = trace.clone();
    linker.instance("wasi:random/random@0.3.0")?.func_wrap(
        "get-random-u64",
        move |_: StoreContextMut<'_, ()>, (): ()| {
            host_trace.lock().unwrap().push("random".into());
            Ok((0u64,))
        },
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    let mut observed = Vec::new();
    for (input, expected) in [(21.0, 84.0), (-1.0, 14.0), (4.0, 16.0)] {
        trace.lock().unwrap().clear();
        let mut pending = Box::pin(run.call_async(&mut store, (input,)));
        assert!(
            timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        assert_eq!(*trace.lock().unwrap(), ["random", "wait:1000000", "random"]);
        release.notify_one();
        assert_eq!(timeout(Duration::from_secs(2), pending).await??.0, expected);
        assert_eq!(
            *trace.lock().unwrap(),
            ["random", "wait:1000000", "random", "random"]
        );
        observed.push((expected, trace.lock().unwrap().clone()));
    }
    let scratch = tempfile::tempdir()?;
    let source_path = scratch.path().join("stored.ts");
    std::fs::write(&source_path, source)?;
    let checked = std::process::Command::new("tsc")
        .current_dir(scratch.path())
        .args([
            "--noEmit", "--strict", "--target", "ES2022", "--module", "esnext",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/types/p3.d.ts"))
        .arg(&source_path)
        .output()?;
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    let node_source = format!(
        "{}\n{source}",
        include_str!("support/node-task-observer.mjs")
    );
    let node_path = scratch.path().join("stored.mts");
    std::fs::write(
        &node_path,
        format!(
            "{node_source}\nconst observed = []; for (const input of [21, -1, 4]) {{ trace.length = 0; observed.push([await run(input), [...trace]]); }} console.log(JSON.stringify(observed));"
        ),
    )?;
    let node = std::process::Command::new("node").arg(node_path).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Vec<(f64, Vec<String>)>>(&node.stdout)?,
        observed
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn nonblocking_import_returns_to_caller_before_completion() -> Result<()> {
    let component = r#"(component
      (import "wait" (func $wait async (param "duration" u64)))
      (import "observe" (func $observe))
      (core module $storage (memory (export "memory") 1))
      (core instance $storage (instantiate $storage))
      (core func $start (canon lower (func $wait) async))
      (core func $observe (canon lower (func $observe)))
      (core func $new (canon waitable-set.new))
      (core func $join (canon waitable.join))
      (core func $wait (canon waitable-set.wait (memory (core memory $storage "memory"))))
      (core func $drop (canon subtask.drop))
      (core func $drop-set (canon waitable-set.drop))
      (core module $probe
        (import "host" "memory" (memory 1))
        (import "host" "start" (func $start (param i64) (result i32)))
        (import "host" "observe" (func $observe))
        (import "host" "new" (func $new (result i32)))
        (import "host" "join" (func $join (param i32 i32)))
        (import "host" "wait" (func $wait (param i32 i32) (result i32)))
        (import "host" "drop" (func $drop (param i32)))
        (import "host" "drop-set" (func $drop-set (param i32)))
        (func (export "run") (result f64) (local $status i32) (local $task i32) (local $set i32)
          (local.set $status (call $start (i64.const 1)))
          (call $observe)
          (if (i32.ne (i32.and (local.get $status) (i32.const 15)) (i32.const 2))
            (then
              (local.set $task (i32.shr_u (local.get $status) (i32.const 4)))
              (local.set $set (call $new))
              (call $join (local.get $task) (local.get $set))
              (loop $pending
                (if (i32.ne (call $wait (local.get $set) (i32.const 0)) (i32.const 1)) (then unreachable))
                (if (i32.ne (i32.load (i32.const 0)) (local.get $task)) (then unreachable))
                (br_if $pending (i32.ne (i32.load (i32.const 4)) (i32.const 2))))
              (call $join (local.get $task) (i32.const 0))
              (call $drop (local.get $task))
              (call $drop-set (local.get $set))))
          (f64.const 42)))
      (core instance $probe (instantiate $probe (with "host" (instance
        (export "memory" (memory $storage "memory"))
        (export "start" (func $start)) (export "observe" (func $observe))
        (export "new" (func $new)) (export "join" (func $join))
        (export "wait" (func $wait)) (export "drop" (func $drop))
        (export "drop-set" (func $drop-set))))))
      (func (export "run") async (result f64) (canon lift (core func $probe "run"))))"#;
    let engine = make_engine()?;
    let component = Component::new(&engine, wat::parse_str(component)?)?;
    let release = Arc::new(Notify::new());
    let trace = Arc::new(Mutex::new(Vec::new()));
    let host_release = release.clone();
    let host_trace = trace.clone();
    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap_concurrent("wait", move |_, (_duration,): (u64,)| {
            let release = host_release.clone();
            let trace = host_trace.clone();
            Box::pin(async move {
                trace.lock().unwrap().push("started");
                release.notified().await;
                trace.lock().unwrap().push("completed");
                Ok(())
            })
        })?;
    let host_trace = trace.clone();
    linker
        .root()
        .func_wrap("observe", move |_: StoreContextMut<'_, ()>, (): ()| {
            host_trace.lock().unwrap().push("caller");
            Ok(())
        })?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    let mut pending = Box::pin(run.call_async(&mut store, ()));
    assert!(
        timeout(Duration::from_millis(10), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(*trace.lock().unwrap(), ["started", "caller"]);
    release.notify_one();
    assert_eq!(timeout(Duration::from_secs(2), pending).await??.0, 42.0);
    assert_eq!(*trace.lock().unwrap(), ["started", "caller", "completed"]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn named_task_starts_eagerly_and_retains_a_single_result() -> Result<()> {
    let component = r#"(component
      (import "wait" (func $wait async (param "duration" u64)))
      (import "observe" (func $observe (param "value" f64)))
      (core module $storage
        (memory (export "memory") 1)
        (table (export "table") 1 funcref))
      (core instance $storage (instantiate $storage))
      (core func $host-wait (canon lower (func $wait)))
      (core func $observe (canon lower (func $observe)))
      (core func $return (canon task.return (result f64)))
      (core func $new (canon waitable-set.new))
      (core func $join (canon waitable.join))
      (core func $wait (canon waitable-set.wait (memory (core memory $storage "memory"))))
      (core func $drop (canon subtask.drop))
      (core func $drop-set (canon waitable-set.drop))
      (core module $guest
        (type $start (func (param f64 i32) (result i32)))
        (import "host" "memory" (memory 1))
        (import "host" "table" (table 1 funcref))
        (import "host" "host-wait" (func $host-wait (param i64)))
        (import "host" "observe" (func $observe (param f64)))
        (import "host" "return" (func $return (param f64)))
        (import "host" "new" (func $new (result i32)))
        (import "host" "join" (func $join (param i32 i32)))
        (import "host" "wait" (func $wait (param i32 i32) (result i32)))
        (import "host" "drop" (func $drop (param i32)))
        (import "host" "drop-set" (func $drop-set (param i32)))
        (func (export "work") (param $value f64)
          (call $observe (f64.const 1))
          (call $host-wait (i64.const 1))
          (call $observe (f64.const 3))
          (call $return (f64.mul (local.get $value) (f64.const 2))))
        (func (export "run") (local $status i32) (local $task i32) (local $set i32)
          (local.set $status (call_indirect (type $start) (f64.const 21) (i32.const 64) (i32.const 0)))
          (call $observe (f64.const 2))
          (if (i32.ne (i32.and (local.get $status) (i32.const 15)) (i32.const 2))
            (then
              (local.set $task (i32.shr_u (local.get $status) (i32.const 4)))
              (local.set $set (call $new))
              (call $join (local.get $task) (local.get $set))
              (loop $pending
                (if (i32.ne (call $wait (local.get $set) (i32.const 0)) (i32.const 1)) (then unreachable))
                (if (i32.ne (i32.load (i32.const 0)) (local.get $task)) (then unreachable))
                (br_if $pending (i32.ne (i32.load (i32.const 4)) (i32.const 2))))
              (call $join (local.get $task) (i32.const 0))
              (call $drop (local.get $task))
              (call $drop-set (local.get $set))))
          (call $return (f64.add (f64.load (i32.const 64)) (f64.load (i32.const 64))))))
      (core instance $guest (instantiate $guest (with "host" (instance
        (export "memory" (memory $storage "memory")) (export "table" (table $storage "table"))
        (export "host-wait" (func $host-wait)) (export "observe" (func $observe))
        (export "return" (func $return))
        (export "new" (func $new)) (export "join" (func $join))
        (export "wait" (func $wait)) (export "drop" (func $drop))
        (export "drop-set" (func $drop-set))))))
      (func $work async (param "value" f64) (result f64) (canon lift (core func $guest "work") async))
      (core func $start-work (canon lower (func $work) async (memory (core memory $storage "memory"))))
      (core module $wire
        (import "host" "table" (table 1 funcref))
        (import "host" "work" (func $work (param f64 i32) (result i32)))
        (elem (i32.const 0) func $work))
      (core instance $wire (instantiate $wire (with "host" (instance
        (export "table" (table $storage "table")) (export "work" (func $start-work))))))
      (func (export "run") async (result f64) (canon lift (core func $guest "run") async)))"#;
    let engine = make_engine()?;
    let component = Component::new(&engine, wat::parse_str(component)?)?;
    let release = Arc::new(Notify::new());
    let trace = Arc::new(Mutex::new(Vec::new()));
    let host_release = release.clone();
    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap_concurrent("wait", move |_, (_duration,): (u64,)| {
            let release = host_release.clone();
            Box::pin(async move {
                release.notified().await;
                Ok(())
            })
        })?;
    let host_trace = trace.clone();
    linker.root().func_wrap(
        "observe",
        move |_: StoreContextMut<'_, ()>, (value,): (f64,)| {
            host_trace.lock().unwrap().push(value);
            Ok(())
        },
    )?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    let mut pending = Box::pin(run.call_async(&mut store, ()));
    assert!(
        timeout(Duration::from_millis(10), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(*trace.lock().unwrap(), [1.0, 2.0]);
    release.notify_one();
    assert_eq!(timeout(Duration::from_secs(2), pending).await??.0, 84.0);
    assert_eq!(*trace.lock().unwrap(), [1.0, 2.0, 3.0]);
    Ok(())
}
