//! Host cancellation of a callback export, using main's independent-component fixture.
use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker},
};

#[path = "../support/cancellation_composition.rs"]
mod composition;

const CALLER: &str = r#"(module
  (import "$root" "[async-lower]operation" (func $start (param i32 i32) (result i32)))
  (import "$root" "[subtask-cancel]" (func $cancel (param i32) (result i32)))
  (import "$root" "[subtask-drop]" (func $drop (param i32)))
  (import "[export]$root" "[task-return]run" (func $return (param i32)))
  (memory (export "memory") 1)
  (func (export "[async-lift]run") (param i32) (result i32)
    (local $task i32) (local $status i32)
    (local.set $status (call $start (local.get 0) (i32.const 32)))
    (local.set $task (i32.shr_u (local.get $status) (i32.const 4)))
    (if (i32.eq (i32.and (local.get $status) (i32.const 15)) (i32.const 2))
      (then (call $return (i32.const 2)) (return (i32.const 0))))
    (local.set $status (call $cancel (local.get $task)))
    (call $drop (local.get $task))
    (call $return (local.get $status))
    (i32.const 0))
  (func (export "[callback][async-lift]run") (param i32 i32 i32) (result i32) unreachable))"#;

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_exports_release_pending_host_calls_before_acknowledging() -> Result<()> {
    let wit = "package test:cancel; interface host {wait:async func(mode:u32)->u32;} world callee {import host;export run:async func(mode:u32)->u32;}";
    let (resolve, package) = perry_wit::component::wit::resolve_source("cancel.wit", wit)?;
    let world = resolve.select_world(&[package], Some("callee"))?;
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:cancel; world caller {import wait:async func(mode:u32)->u32;import operation:async func(mode:u32)->u32;export run:async func(mode:u32)->u32;}",
    )?;
    let caller = perry_wit::component::embed_and_encode(
        &wat::parse_str(CALLER)?,
        directory.path(),
        Some("caller"),
    )?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_threading(false)
        .wasm_component_model_async_stackful(false)
        .wasm_component_model_more_async_builtins(false);
    let engine = Engine::new(&config)?;
    for body in [
        "return await wait(mode);",
        "const a=wait(mode);const b=wait(mode);return (await Promise.all([a,b]))[0];",
    ] {
        let source = format!(
            "import {{wait}} from 'test:cancel/host'; export async function run(mode:number):Promise<number>{{{body}}}"
        );
        let callee = perry_wit::compile_for_world(&source, "cancel.ts", &resolve, world, true)?
            .stripped
            .unwrap();
        let component = Component::new(
            &engine,
            composition::compose_with_interface(&callee, &caller, Some("test:cancel/host"))?,
        )?;
        let mut linker = Linker::new(&engine);
        let active = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let operations = active.clone();
        let count = started.clone();
        linker
            .root()
            .func_wrap_concurrent("wait", move |_, (_,): (u32,)| {
                count.fetch_add(1, Ordering::SeqCst);
                operations.fetch_add(1, Ordering::SeqCst);
                let active = Active(operations.clone());
                Box::pin(async move {
                    let _active = active;
                    std::future::pending::<()>().await;
                    Ok((42u32,))
                })
            })?;
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
        for _ in 0..20 {
            let (status,) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                run.call_async(&mut store, (0,)),
            )
            .await??;
            assert_eq!(status, 4);
            assert_eq!(active.load(Ordering::SeqCst), 0);
            store.assert_concurrent_state_empty();
        }
        assert!(started.load(Ordering::SeqCst) >= 20);
    }
    Ok(())
}
