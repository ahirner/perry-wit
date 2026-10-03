//! Native subtask protocol probes and stored Promise integration tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;
use tokio::time::timeout;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreContextMut};

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
    let mut config = Config::new();
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
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
    let mut config = Config::new();
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
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
