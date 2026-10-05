//! Controlled composed host cancellation exercises the shared operation registry.
use super::*;
use std::collections::BTreeMap;
use waffle::{Export, ExportKind, MemoryData, TableData};

fn callee() -> Result<Vec<u8>> {
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
    let imports = declare(&mut module);
    let start = builder::native(&mut module, "[async-lower]wait", &[I32; 2], &[I32]);
    let new_thread = builder::native(&mut module, "[thread-new-indirect-v0]", &[I32; 2], &[I32]);
    let resume = builder::native(&mut module, "[thread-yield-then-resume]", &[I32], &[I32]);
    let returns = crate::waffle_backend::runtime::imports::declare_imports(
        &mut module,
        "[export]$root",
        &[
            crate::waffle_backend::runtime::imports::Function {
                name: "[task-return]run".into(),
                params: vec!["i32"],
                results: vec![],
            },
            crate::waffle_backend::runtime::imports::Function {
                name: "[task-cancel]".into(),
                params: vec![],
                results: vec![],
            },
        ],
    );
    let allocator = crate::waffle_backend::allocation::emit_allocator(&mut module, memory, 1024)?;
    let operations = emit(&mut module, memory, allocator, imports, &[])?;
    let worker = builder::declare(&mut module, "worker", &[I32], &[]);
    let table = module.tables.push(TableData {
        ty: waffle::Type::FuncRef,
        initial: 1,
        max: Some(1),
        func_elements: Some(vec![worker]),
    });
    module.exports.push(Export {
        name: "__indirect_function_table".into(),
        kind: ExportKind::Table(table),
    });
    let mut b = Builder::new(&module, worker, memory);
    let pointer = b.integer(208);
    let packed = b.call(start, &[b.param(0), pointer], &[I32])[0];
    let node = b.call(operations.register, &[packed], &[I32])[0];
    let status = b.call(operations.wait, &[node], &[I32])[0];
    let two = b.integer(2);
    let valid = b.op(O::I32GeU, &[status, two], I32);
    b.require(valid);
    let address = b.integer(204);
    let one = b.integer(1);
    b.store(address, 0, one, I32);
    b.ret(&[]);
    b.finish(&mut module, worker)?;

    let publish = builder::declare(&mut module, "publish", &[], &[I32]);
    let mut b = Builder::new(&module, publish, memory);
    let address = b.integer(204);
    let done = b.load(address, 0, I32);
    let finish = b.body.add_block();
    let pending = b.body.add_block();
    b.branch(done, finish, pending);
    b.block = pending;
    let action = b.call(operations.action, &[], &[I32])[0];
    b.ret(&[action]);
    b.block = finish;
    let cancelled = b.call(operations.finish, &[], &[I32])[0];
    let cancel = b.body.add_block();
    let success = b.body.add_block();
    let cleanup = b.body.add_block();
    b.branch(cancelled, cancel, success);
    b.block = cancel;
    b.call(returns["[task-cancel]"], &[], &[]);
    b.jump(cleanup, &[]);
    b.block = success;
    let address = b.integer(208);
    let value = b.load(address, 0, I32);
    b.call(returns["[task-return]run"], &[value], &[]);
    b.jump(cleanup, &[]);
    b.block = cleanup;
    b.call(allocator.post_return, &[], &[]);
    let zero = b.integer(0);
    b.ret(&[zero]);
    b.finish(&mut module, publish)?;

    let entry = builder::declare(&mut module, "[async-lift]run", &[I32], &[I32]);
    let mut b = Builder::new(&module, entry, memory);
    b.call(operations.enter, &[], &[]);
    let address = b.integer(204);
    let zero = b.integer(0);
    b.store(address, 0, zero, I32);
    let worker = b.call(new_thread, &[zero, b.param(0)], &[I32])[0];
    b.call(resume, &[worker], &[I32]);
    let action = b.call(operations.action, &[], &[I32])[0];
    b.ret(&[action]);
    b.finish(&mut module, entry)?;
    let callback = builder::declare(&mut module, "[callback][async-lift]run", &[I32; 3], &[I32]);
    let mut b = Builder::new(&module, callback, memory);
    let six = b.integer(6);
    let cancelled = b.op(O::I32Eq, &[b.param(0), six], I32);
    let cancel = b.body.add_block();
    let inspect = b.body.add_block();
    let finish = b.body.add_block();
    b.branch(cancelled, cancel, inspect);
    b.block = cancel;
    b.call(operations.cancel_all, &[], &[]);
    b.jump(finish, &[]);
    b.block = inspect;
    let notify = b.body.add_block();
    b.branch(b.param(0), notify, finish);
    b.block = notify;
    b.call(
        operations.notify,
        &[b.param(0), b.param(1), b.param(2)],
        &[],
    );
    b.jump(finish, &[]);
    b.block = finish;
    let action = b.call(publish, &[], &[I32])[0];
    b.ret(&[action]);
    b.finish(&mut module, callback)?;
    for (name, func) in BTreeMap::from([
        ("[async-lift]run", entry),
        ("[callback][async-lift]run", callback),
    ]) {
        module.exports.push(Export {
            name: name.into(),
            kind: ExportKind::Func(func),
        });
    }
    let (resolve, package) = crate::component::wit::resolve_source(
        "package test:cancel; world callee { import wait:async func(mode:u32)->u32; export run:async func(mode:u32)->u32; }",
    )?;
    let world = resolve.select_world(&[package], Some("callee"))?;
    crate::component::encode_resolved(&module.to_wasm_bytes()?, &resolve, world)
}

#[path = "../../../tests/support/cancellation_composition.rs"]
mod composition;
#[tokio::test(flavor = "current_thread")]
async fn registered_operations_acknowledge_cancellation_and_release_waiters() -> Result<()> {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Config, Engine, Store, StoreLimitsBuilder};
    struct Pending(Arc<AtomicUsize>);
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let (resolve, package) = crate::component::wit::resolve_source(
        "package test:cancel; world caller { import wait:async func(mode:u32)->u32; import operation:async func(mode:u32)->u32; export run:async func(mode:u32)->u32; }",
    )?;
    let world = resolve.select_world(&[package], Some("caller"))?;
    let caller = crate::component::encode_resolved(
        &wat::parse_str(include_str!(
            "../../../tests/fixtures/cancellation/caller.wat"
        ))?,
        &resolve,
        world,
    )?;
    let bytes = composition::compose(&callee()?, &caller)?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, bytes)?;
    let active = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let completion = Arc::new(tokio::sync::Notify::new());
    let mut linker = Linker::new(&engine);
    let observed = active.clone();
    let starts = started.clone();
    let finishes = completed.clone();
    linker
        .root()
        .func_wrap_concurrent("wait", move |_, (mode,): (u32,)| {
            let (active, started, completed, release, completion) = (
                observed.clone(),
                starts.clone(),
                finishes.clone(),
                release.clone(),
                completion.clone(),
            );
            Box::pin(async move {
                if mode == 3 {
                    release.notify_one();
                    completion.notified().await;
                    return Ok((0u32,));
                }
                active.fetch_add(1, Ordering::SeqCst);
                started.fetch_add(1, Ordering::SeqCst);
                let _pending = Pending(active);
                if mode == 0 {
                    std::future::pending::<()>().await;
                }
                if mode == 2 {
                    release.notified().await;
                }
                completed.fetch_add(1, Ordering::SeqCst);
                if mode == 2 {
                    completion.notify_one();
                }
                Ok((42u32,))
            })
        })?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65536).build(),
    );
    store.limiter(|limits| limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
    for _ in 0..100 {
        for mode in [0, 1, 2] {
            let status =
                tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, (mode,)))
                    .await??
                    .0;
            assert_eq!(status, if mode == 1 { 2 } else { 4 }, "mode={mode}");
            assert_eq!(active.load(Ordering::SeqCst), 0);
            store.assert_concurrent_state_empty();
        }
    }
    assert_eq!(started.load(Ordering::SeqCst), 300);
    assert_eq!(completed.load(Ordering::SeqCst), 200);
    Ok(())
}
