//! Native cancellation acknowledgment against the pinned Component Model runtime.
#[path = "support/cancellation_composition.rs"]
mod cancellation_composition;

use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::{sync::Notify, time::timeout};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

#[derive(Default)]
struct Operations {
    active: AtomicUsize,
    started: AtomicUsize,
    completed: AtomicUsize,
    release: Notify,
    completion: Notify,
}
struct Owner(Arc<Operations>);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_subtask_cancellation_is_acknowledged_before_owner_release() -> Result<()> {
    let wit = tempfile::tempdir()?;
    std::fs::write(
        wit.path().join("world.wit"),
        "package test:cancel; world probe { import wasi:clocks/monotonic-clock@0.3.0; export run:async func(mode:u32)->u32; }",
    )?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    for asynchronous in [false, true] {
        let cancellation = if asynchronous {
            "[async-lower][subtask-cancel]"
        } else {
            "[subtask-cancel]"
        };
        let core = wat::parse_str(format!(
            r#"(module
          (import "wasi:clocks/monotonic-clock@0.3.0" "[async-lower]wait-for" (func $start (param i64) (result i32)))
          (import "wasi:clocks/monotonic-clock@0.3.0" "wait-for" (func $checkpoint (param i64)))
          (import "$root" "{cancellation}" (func $cancel (param i32) (result i32)))
          (import "$root" "[subtask-drop]" (func $drop (param i32)))
          (import "$root" "[waitable-set-new]" (func $new-set (result i32)))
          (import "$root" "[waitable-set-drop]" (func $drop-set (param i32)))
          (import "$root" "[waitable-join]" (func $join (param i32 i32)))
          (import "$root" "[waitable-set-wait]" (func $wait (param i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "run") (param $mode i32) (result i32)
            (local $status i32) (local $task i32) (local $set i32)
            (local.set $status (call $start (i64.extend_i32_u (local.get $mode))))
            (if (i32.eq (i32.and (local.get $status) (i32.const 15)) (i32.const 2))
              (then (return (i32.const 2))))
            (local.set $task (i32.shr_u (local.get $status) (i32.const 4)))
            (if (i32.eq (local.get $mode) (i32.const 2)) (then (call $checkpoint (i64.const 3))))
            (local.set $status (call $cancel (local.get $task)))
            (if (i32.eq (local.get $status) (i32.const -1))
              (then
                (local.set $set (call $new-set))
                (call $join (local.get $task) (local.get $set))
                (loop $pending
                  (if (i32.ne (call $wait (local.get $set) (i32.const 0)) (i32.const 1)) (then unreachable))
                  (if (i32.ne (i32.load (i32.const 0)) (local.get $task)) (then unreachable))
                  (local.set $status (i32.load (i32.const 4)))
                  (br_if $pending (i32.lt_u (local.get $status) (i32.const 2))))
                (call $join (local.get $task) (i32.const 0))
                (call $drop-set (local.get $set))))
            (call $drop (local.get $task))
            (local.get $status)))"#
        ))?;
        let bytes = perry_wit::component::embed_and_encode(&core, wit.path(), Some("probe"))?;
        let component = Component::new(&engine, bytes)?;
        let operations = Arc::new(Operations::default());
        let mut linker = Linker::new(&engine);
        let observed = operations.clone();
        linker
            .instance("wasi:clocks/monotonic-clock@0.3.0")?
            .func_wrap_concurrent("wait-for", move |_, (mode,): (u64,)| {
                let operations = observed.clone();
                if mode == 3 {
                    return Box::pin(async move {
                        operations.release.notify_one();
                        operations.completion.notified().await;
                        Ok(())
                    });
                }
                operations.active.fetch_add(1, Ordering::SeqCst);
                operations.started.fetch_add(1, Ordering::SeqCst);
                let owner = Owner(operations.clone());
                Box::pin(async move {
                    let _owner = owner;
                    if mode == 0 {
                        std::future::pending::<()>().await;
                    }
                    if mode == 2 {
                        operations.release.notified().await;
                    }
                    operations.completed.fetch_add(1, Ordering::SeqCst);
                    if mode == 2 {
                        operations.completion.notify_one();
                    }
                    Ok(())
                })
            })?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
        for _ in 0..100 {
            for mode in [0, 1, 2] {
                let status = timeout(Duration::from_secs(5), run.call_async(&mut store, (mode,)))
                    .await??
                    .0;
                assert_eq!(
                    status,
                    if mode == 0 { 4 } else { 2 },
                    "async={asynchronous}, mode={mode}"
                );
                assert_eq!(operations.active.load(Ordering::SeqCst), 0);
                store.assert_concurrent_state_empty();
            }
        }
        assert_eq!(operations.started.load(Ordering::SeqCst), 300);
        assert_eq!(operations.completed.load(Ordering::SeqCst), 200);
    }
    Ok(())
}

fn compose_cancellation_probe() -> Result<Vec<u8>> {
    let wit = tempfile::tempdir()?;
    let definition = wit.path().join("world.wit");
    std::fs::write(
        &definition,
        "package test:cancel; world callee { import wait:async func(mode:u32)->u32; export run:async func(mode:u32)->u32; }",
    )?;
    let callee = perry_wit::component::embed_and_encode(
        &wat::parse_str(include_str!("fixtures/cancellation/callee.wat"))?,
        wit.path(),
        Some("callee"),
    )?;
    std::fs::write(
        &definition,
        "package test:cancel; world caller { import wait:async func(mode:u32)->u32; import operation:async func(mode:u32)->u32; export run:async func(mode:u32)->u32; }",
    )?;
    let caller = perry_wit::component::embed_and_encode(
        &wat::parse_str(include_str!("fixtures/cancellation/caller.wat"))?,
        wit.path(),
        Some("caller"),
    )?;
    cancellation_composition::compose(&callee, &caller)
}

#[tokio::test(flavor = "current_thread")]
async fn component_cancellation_reaches_callback_and_releases_native_owner() -> Result<()> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compose_cancellation_probe()?)?;
    let operations = Arc::new(Operations::default());
    let mut linker = Linker::new(&engine);
    let observed = operations.clone();
    linker
        .root()
        .func_wrap_concurrent("wait", move |_, (mode,): (u32,)| {
            let operations = observed.clone();
            Box::pin(async move {
                if mode == 3 {
                    operations.release.notify_one();
                    operations.completion.notified().await;
                    return Ok((0u32,));
                }
                operations.active.fetch_add(1, Ordering::SeqCst);
                operations.started.fetch_add(1, Ordering::SeqCst);
                let _owner = Owner(operations.clone());
                if mode == 0 {
                    std::future::pending::<()>().await;
                }
                if mode == 2 {
                    operations.release.notified().await;
                }
                operations.completed.fetch_add(1, Ordering::SeqCst);
                if mode == 2 {
                    operations.completion.notify_one();
                }
                Ok((42u32,))
            })
        })?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
    for _ in 0..100 {
        for mode in [0, 1, 2] {
            let status = timeout(Duration::from_secs(5), run.call_async(&mut store, (mode,)))
                .await??
                .0;
            assert_eq!(status, if mode == 1 { 2 } else { 4 }, "mode={mode}");
            assert_eq!(operations.active.load(Ordering::SeqCst), 0);
            store.assert_concurrent_state_empty();
        }
    }
    assert_eq!(operations.started.load(Ordering::SeqCst), 300);
    assert_eq!(operations.completed.load(Ordering::SeqCst), 200);
    Ok(())
}

fn compile_cancellable(source: &str) -> Result<Vec<u8>> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("cancel.wit", "package test:cancel; interface host {wait:async func(mode:u32)->u32;} world callee {import host; export run:async func(mode:u32)->u32;}")?;
    let world = resolve.select_world(&[package], Some("callee"))?;
    let compiled = perry_wit::waffle_backend::compile_typescript_for_world(
        source,
        "cancel.ts",
        &Default::default(),
        resolve,
        world,
    )?;
    let wit = tempfile::tempdir()?;
    std::fs::write(
        wit.path().join("world.wit"),
        "package test:cancel; world caller { import wait:async func(mode:u32)->u32; import operation:async func(mode:u32)->u32; export run:async func(mode:u32)->u32; }",
    )?;
    let caller = perry_wit::component::embed_and_encode(
        &wat::parse_str(include_str!("fixtures/cancellation/caller.wat"))?,
        wit.path(),
        Some("caller"),
    )?;
    cancellation_composition::compose_with_interface(
        &compiled.component.unwrap(),
        &caller,
        Some("test:cancel/host"),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn compiled_exports_acknowledge_host_cancellation_and_run_finally() -> Result<()> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    for body in [
        "return await wait(mode);",
        "const first = wait(mode); const second = wait(1); const values = await Promise.all([first, second]); return values[0];",
        "const first = wait(mode); const second = wait(1); const result = await Promise.race([first, second]); await first; return result;",
    ] {
        let source = format!(
            "import {{wait}} from 'test:cancel/host'; let running = 0; export async function run(mode:number):Promise<number> {{ if (running !== 0) throw 99; running = 1; try {{ {body} }} finally {{ running = 0; await wait(4); }} }}"
        );
        let component = Component::new(&engine, compile_cancellable(&source)?)?;
        let operations = Arc::new(Operations::default());
        let cleanup = Arc::new(AtomicUsize::new(0));
        let mut linker = Linker::new(&engine);
        let observed = operations.clone();
        let cleanups = cleanup.clone();
        linker
            .root()
            .func_wrap_concurrent("wait", move |_, (mode,): (u32,)| {
                let operations = observed.clone();
                let cleanups = cleanups.clone();
                Box::pin(async move {
                    if mode == 4 {
                        cleanups.fetch_add(1, Ordering::SeqCst);
                        return Ok((0u32,));
                    }
                    if mode == 3 {
                        operations.release.notify_one();
                        operations.completion.notified().await;
                        return Ok((0u32,));
                    }
                    operations.active.fetch_add(1, Ordering::SeqCst);
                    operations.started.fetch_add(1, Ordering::SeqCst);
                    let _owner = Owner(operations.clone());
                    if mode == 0 {
                        std::future::pending::<()>().await;
                    }
                    if mode == 2 {
                        operations.release.notified().await;
                    }
                    operations.completed.fetch_add(1, Ordering::SeqCst);
                    if mode == 2 {
                        operations.completion.notify_one();
                    }
                    Ok((42u32,))
                })
            })?;
        let mut store = Store::new(
            &engine,
            wasmtime::StoreLimitsBuilder::new()
                .memory_size(65536)
                .build(),
        );
        store.limiter(|limits| limits);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
        for round in 0..30 {
            for mode in [0, 1, 2] {
                let status = timeout(Duration::from_secs(5), run.call_async(&mut store, (mode,)))
                    .await?
                    .map_err(|error| {
                        anyhow::anyhow!("mode={mode}, round={round}, source={source}: {error:#}")
                    })?
                    .0;
                assert_eq!(
                    status,
                    if mode == 1 { 2 } else { 4 },
                    "mode={mode}, round={round}, source={source}"
                );
                assert_eq!(operations.active.load(Ordering::SeqCst), 0);
                assert_eq!(
                    cleanup.load(Ordering::SeqCst),
                    round * 3 + mode as usize + 1
                );
                store.assert_concurrent_state_empty();
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn compiled_empty_race_can_be_cancelled_without_native_work() -> Result<()> {
    let source = "export async function run(mode:number):Promise<number> { if (mode === 1) return 42; const empty:Promise<number>[] = []; return await Promise.race(empty); }";
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compile_cancellable(source)?)?;
    let mut linker = Linker::new(&engine);
    linker
        .root()
        .func_wrap_concurrent("wait", |_, (_mode,): (u32,)| {
            Box::pin(async { Ok((0u32,)) })
        })?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
    for _ in 0..100 {
        for mode in [0, 1] {
            let status = timeout(Duration::from_secs(5), run.call_async(&mut store, (mode,)))
                .await??
                .0;
            assert_eq!(status, if mode == 1 { 2 } else { 4 });
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}
