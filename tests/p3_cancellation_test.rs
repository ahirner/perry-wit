//! Native cancellation acknowledgment against the pinned Component Model runtime.

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
    use wasm_encoder::{
        Alias, ComponentAliasSection, ComponentExportKind as Kind, ComponentExportSection,
        ComponentImportSection, ComponentInstanceSection, ComponentSectionId, ComponentTypeRef,
        ComponentTypeSection, PrimitiveValType, RawSection,
    };
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
    // Test-only composition of two independently encoded application components.
    let mut component = wasm_encoder::Component::new();
    let mut types = ComponentTypeSection::new();
    types
        .function()
        .async_(true)
        .params([("mode", PrimitiveValType::U32)])
        .result(Some(PrimitiveValType::U32.into()));
    component.section(&types);
    let mut imports = ComponentImportSection::new();
    imports.import("wait", ComponentTypeRef::Func(0));
    component.section(&imports);
    for bytes in [&callee, &caller] {
        component.section(&RawSection {
            id: ComponentSectionId::Component.into(),
            data: bytes,
        });
    }
    let mut instances = ComponentInstanceSection::new();
    instances.instantiate(0, [("wait", Kind::Func, 0)]);
    component.section(&instances);
    let mut aliases = ComponentAliasSection::new();
    aliases.alias(Alias::InstanceExport {
        instance: 0,
        kind: Kind::Func,
        name: "run",
    });
    component.section(&aliases);
    let mut instances = ComponentInstanceSection::new();
    instances.instantiate(1, [("wait", Kind::Func, 0), ("operation", Kind::Func, 1)]);
    component.section(&instances);
    let mut aliases = ComponentAliasSection::new();
    aliases.alias(Alias::InstanceExport {
        instance: 1,
        kind: Kind::Func,
        name: "run",
    });
    component.section(&aliases);
    let mut exports = ComponentExportSection::new();
    exports.export("run", Kind::Func, 2, None);
    component.section(&exports);
    Ok(component.finish())
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
