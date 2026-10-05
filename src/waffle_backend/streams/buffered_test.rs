use std::collections::BTreeMap;
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::timeout;
use waffle::{Export, ExportKind, MemoryData, Module};
use wasmtime::component::{Component, Instance, Linker, StreamReader};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

use crate::waffle_backend::{allocation, runtime};

use crate::waffle_backend::test_input::{ControlledProducer, Observations};

async fn instantiate(cap: usize) -> Result<(Store<StoreLimits>, Instance)> {
    let mut module = Module::empty();
    let memory = module.memories.push(MemoryData {
        initial_pages: 1,
        maximum_pages: None,
        segments: vec![],
    });
    module.exports.push(Export {
        name: "memory".into(),
        kind: ExportKind::Memory(memory),
    });
    let imports = super::declare_imports(&mut module);
    let allocator = allocation::emit_allocator(&mut module, memory, 1024)?;
    let transfer = super::emit_read_transfer(&mut module, memory, imports.read)?;
    let buffered = super::buffered::emit(&mut module, memory, allocator, transfer)?;
    let wrappers = runtime::emit_functions(
        &mut module,
        memory,
        r#"(module
      (import "host" "buffered" (func $buffered (param i32 i32) (result i32 i32 i32)))
      (import "host" "drop" (func $drop (param i32)))
      (import "host" "post-return" (func $post-return))
      (memory 1)
      (func (export "run") (param $input i32) (param $limit i32) (result i32)
        (local $error i32) (local $data i32) (local $length i32)
        (call $buffered (local.get $input) (local.get $limit))
        local.set $length local.set $data local.set $error
        (call $drop (local.get $input))
        (i32.store (i32.const 512) (local.get $error))
        (i32.store (i32.const 516) (if (result i32) (local.get $error) (then (i32.const 8)) (else (local.get $data))))
        (i32.store (i32.const 520) (local.get $length))
        (i32.const 512))
      (func (export "cabi_post_run") (param i32) (call $post-return)))"#,
        &BTreeMap::from([
            ("buffered", buffered),
            ("drop", imports.drop),
            ("post-return", allocator.post_return),
        ]),
    )?;
    for (name, function) in wrappers {
        module.exports.push(Export {
            name,
            kind: ExportKind::Func(function),
        });
    }
    let (resolve, package) = crate::component::wit::resolve_source(
        "package test:buffered-stream; world fixture {
            export run: async func(input: stream<u8>, limit: u32) -> result<list<u8>, u32>;
        }",
    )?;
    let world = resolve.select_world(&[package], Some("fixture"))?;
    let component =
        crate::waffle_backend::encode_component(&module.to_wasm_bytes()?, resolve, world)?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, component)?;
    let mut store = Store::new(&engine, StoreLimitsBuilder::new().memory_size(cap).build());
    store.limiter(|limits| limits);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    Ok((store, instance))
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_native_reads_preserve_bytes_and_detect_exact_limits() -> Result<()> {
    let (mut store, instance) = instantiate(65_536).await?;
    let run = instance
        .get_typed_func::<(StreamReader<u8>, u32), (Result<Vec<u8>, u32>,)>(&mut store, "run")?;
    for _ in 0..20 {
        for limit in [0_u32, 1, 31, 8191, 8192, 8193, 16384, 16385] {
            for size in [0, limit.saturating_sub(1), limit, limit + 1] {
                let bytes: Vec<u8> = (0..size)
                    .map(|index| (index * 37 + index / 251) as u8)
                    .collect();
                let expected = if size > limit {
                    Err(8)
                } else {
                    Ok(bytes.clone())
                };
                let input = StreamReader::new(&mut store, bytes)?;
                assert_eq!(
                    run.call_async(&mut store, (input, limit)).await?.0,
                    expected,
                    "limit={limit}, size={size}"
                );
                store.assert_concurrent_state_empty();
            }
        }
    }
    let input = StreamReader::new(&mut store, vec![0, 128, 255])?;
    assert_eq!(
        run.call_async(&mut store, (input, u32::MAX)).await?.0,
        Ok(vec![0, 128, 255])
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_read_supports_the_http_consumers_four_mib_limit() -> Result<()> {
    let (mut store, instance) = instantiate(16 * 1024 * 1024).await?;
    let run = instance
        .get_typed_func::<(StreamReader<u8>, u32), (Result<Vec<u8>, u32>,)>(&mut store, "run")?;
    let limit = 4 * 1024 * 1024;
    for size in [limit - 1, limit, limit + 1, limit] {
        let bytes: Vec<u8> = (0..size)
            .map(|index| (index * 37 + index / 251) as u8)
            .collect();
        let expected = if size > limit {
            Err(8)
        } else {
            Ok(bytes.clone())
        };
        let input = StreamReader::new(&mut store, bytes)?;
        assert_eq!(
            run.call_async(&mut store, (input, limit)).await?.0,
            expected
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn full_buffers_wait_for_eof_and_overflow_releases_the_pending_producer() -> Result<()> {
    let (mut store, instance) = instantiate(65_536).await?;
    let run = instance
        .get_typed_func::<(StreamReader<u8>, u32), (Result<Vec<u8>, u32>,)>(&mut store, "run")?;
    for overflow in [false, true, false] {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(2);
        sender.send(Ok(vec![])).await?;
        sender.send(Ok(vec![0, 255, 128])).await?;
        let input = StreamReader::new(
            &mut store,
            ControlledProducer {
                receiver,
                observations: observations.clone(),
            },
        )?;
        let mut invocation = Box::pin(run.call_async(&mut store, (input, 3)));
        tokio::select! {
            result = &mut invocation => panic!("returned before EOF: {result:?}"),
            () = observations.pending.notified() => {}
        }
        assert!(
            timeout(Duration::from_millis(10), &mut invocation)
                .await
                .is_err()
        );
        if overflow {
            sender.send(Ok(vec![17])).await?;
            assert_eq!(
                timeout(Duration::from_secs(5), invocation).await??.0,
                Err(8)
            );
        } else {
            drop(sender);
            assert_eq!(
                timeout(Duration::from_secs(5), invocation).await??.0,
                Ok(vec![0, 255, 128])
            );
        }
        assert!(observations.dropped.load(Ordering::SeqCst));
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn eof_probe_traps_and_disposal_release_the_buffered_producer() -> Result<()> {
    for host_error in [false, true] {
        let (mut store, instance) = instantiate(65_536).await?;
        let run = instance.get_typed_func::<(StreamReader<u8>, u32), (Result<Vec<u8>, u32>,)>(
            &mut store, "run",
        )?;
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(1);
        sender.send(Ok(vec![0, 255, 128])).await?;
        let input = StreamReader::new(
            &mut store,
            ControlledProducer {
                receiver,
                observations: observations.clone(),
            },
        )?;
        let mut invocation = Box::pin(run.call_async(&mut store, (input, 3)));
        tokio::select! {
            result = &mut invocation => panic!("returned before EOF: {result:?}"),
            () = observations.pending.notified() => {}
        }
        if host_error {
            sender.send(Err("body transport failed".into())).await?;
            let error = timeout(Duration::from_secs(5), invocation)
                .await?
                .unwrap_err();
            assert!(format!("{error:#}").contains("body transport failed"));
        } else {
            drop(invocation);
        }
        drop(store);
        assert!(observations.dropped.load(Ordering::SeqCst));
        assert!(sender.is_closed());
    }
    Ok(())
}
