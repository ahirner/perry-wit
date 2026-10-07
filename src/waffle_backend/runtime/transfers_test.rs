//! Native stream/future acknowledgements share the subtask owner registry.
use super::*;
use crate::waffle_backend::runtime::imports;
use crate::waffle_backend::runtime::scheduler::{self, HostTurn};
use waffle::{Export, ExportKind, MemoryData, TableData};

#[derive(Clone, Copy)]
enum Probe {
    StreamRead,
    StreamWrite,
    FutureRead,
    FutureWrite,
}
impl Probe {
    fn stream(self) -> bool {
        matches!(self, Self::StreamRead | Self::StreamWrite)
    }
    fn reading(self) -> bool {
        matches!(self, Self::StreamRead | Self::FutureRead)
    }
    fn family(self) -> &'static str {
        if self.stream() { "stream" } else { "future" }
    }
    fn event(self) -> u32 {
        match self {
            Self::StreamRead => 2,
            Self::StreamWrite => 3,
            Self::FutureRead => 4,
            Self::FutureWrite => 5,
        }
    }
}

fn component(probe: Probe) -> Result<Vec<u8>> {
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
    let i = declare(&mut module);
    let transfer_name = format!(
        "[async-lower][{}-{}-0]run",
        probe.family(),
        if probe.reading() { "read" } else { "write" }
    );
    let cancel_name = format!(
        "[async-lower][{}-cancel-{}-0]run",
        probe.family(),
        if probe.reading() { "read" } else { "write" }
    );
    let new_name = format!("[{}-new-0]run", probe.family());
    let drop_read = format!("[{}-drop-readable-0]run", probe.family());
    let drop_write = format!("[{}-drop-writable-0]run", probe.family());
    let native = imports::declare_imports(
        &mut module,
        "[export]$root",
        &[
            imports::Function {
                name: transfer_name.clone(),
                params: vec!["i32"; if probe.stream() { 3 } else { 2 }],
                results: vec!["i32"],
            },
            imports::Function {
                name: cancel_name.clone(),
                params: vec!["i32"],
                results: vec!["i32"],
            },
            imports::Function {
                name: new_name.clone(),
                params: vec![],
                results: vec!["i64"],
            },
            imports::Function {
                name: drop_read.clone(),
                params: vec!["i32"],
                results: vec![],
            },
            imports::Function {
                name: drop_write.clone(),
                params: vec!["i32"],
                results: vec![],
            },
            imports::Function {
                name: "[task-return]run".into(),
                params: vec!["i32"],
                results: vec![],
            },
            imports::Function {
                name: "[task-cancel]".into(),
                params: vec![],
                results: vec![],
            },
        ],
    );
    let spawn = builder::native(&mut module, "[thread-new-indirect-v0]", &[I32; 2], &[I32]);
    let resume = builder::native(&mut module, "[thread-yield-then-resume]", &[I32], &[I32]);
    let allocator = crate::waffle_backend::allocation::emit_allocator(&mut module, memory, 1024)?;
    let operations = emit(
        &mut module,
        memory,
        allocator,
        i,
        &[Transfer {
            event: probe.event(),
            cancellation: Cancellation::Cancel(native[&cancel_name]),
        }],
    )?;
    let worker = builder::declare(&mut module, "transfer.worker", &[I32], &[]);
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
    let handle = b.param(0);
    let zero = b.integer(0);
    let one = b.integer(1);
    let mode_address = b.integer(208);
    let mode = b.load(mode_address, 0, I32);
    let cancel = b.op(O::I32Eqz, &[mode], I32);
    let frame = b.call(allocator.frame_new, &[one], &[I32])[0];
    let buffer = b.allocate(allocator.realloc, 4096, 4);
    b.store(frame, 12, buffer, I32);
    let value = b.integer(42);
    b.store(buffer, 0, value, I32);
    let capacity = b.integer(4096);
    let next = b.body.add_block();
    let total = b.body.add_blockparam(next, I32);
    b.jump(next, &[zero]);
    b.block = next;
    let mut args = vec![handle, buffer];
    if probe.stream() {
        args.push(capacity);
    }
    let initial = b.call(native[&transfer_name], &args, &[I32])[0];
    let owner = b.call(
        operations.register_transfer.unwrap(),
        &[handle, initial, one],
        &[I32],
    )[0];
    let cancelled = b.body.add_block();
    let wait = b.body.add_block();
    b.branch(cancel, cancelled, wait);
    b.block = cancelled;
    b.call(operations.cancel, &[owner], &[]);
    b.jump(wait, &[]);
    b.block = wait;
    let status = b.call(operations.wait, &[owner], &[I32])[0];
    let result = if probe.stream() && probe.reading() {
        let mask = b.integer(15);
        let flag = b.op(O::I32And, &[status, mask], I32);
        let four = b.integer(4);
        let count = b.op(O::I32ShrU, &[status, four], I32);
        let total = b.op(O::I32Add, &[total, count], I32);
        let done = b.body.add_block();
        let again = b.body.add_block();
        b.branch(flag, done, again);
        b.block = again;
        b.jump(next, &[total]);
        b.block = done;
        b.op(O::Select, &[status, total, cancel], I32)
    } else if probe.reading() {
        let value = b.load(buffer, 0, I32);
        b.op(O::Select, &[status, value, cancel], I32)
    } else {
        status
    };
    if matches!(probe, Probe::FutureWrite) {
        let address = b.integer(212);
        let reader = b.load(address, 0, I32);
        b.call(native[&drop_read], &[reader], &[]);
        // Cancellation preserves an unwritten future. Supply its value after
        // closing this probe's reader before releasing the writable endpoint.
        let final_status = b.call(native[&transfer_name], &[handle, buffer], &[I32])[0];
        let dropped = b.op(O::I32Eq, &[final_status, one], I32);
        b.require(dropped);
    }
    b.call(
        native[if probe.reading() {
            &drop_read
        } else {
            &drop_write
        }],
        &[handle],
        &[],
    );
    if matches!(probe, Probe::StreamWrite) {
        let address = b.integer(212);
        let reader = b.load(address, 0, I32);
        b.call(native[&drop_read], &[reader], &[]);
    }
    b.call(allocator.frame_drop, &[frame], &[]);
    let address = b.integer(216);
    b.store(address, 0, result, I32);
    let address = b.integer(204);
    b.store(address, 0, one, I32);
    b.ret(&[]);
    b.finish(&mut module, worker)?;

    let entry = builder::declare(
        &mut module,
        "[async-lift]run",
        if probe.reading() { &[I32, I32] } else { &[I32] },
        &[I32],
    );
    let mut b = Builder::new(&module, entry, memory);
    b.call(operations.enter, &[], &[]);
    let zero = b.integer(0);
    let address = b.integer(204);
    b.store(address, 0, zero, I32);
    let address = b.integer(208);
    b.store(address, 0, b.param(0), I32);
    let input = if probe.reading() {
        b.param(1)
    } else {
        let pair = b.call(native[&new_name], &[], &[waffle::Type::I64])[0];
        let reader = b.op(O::I32WrapI64, &[pair], I32);
        let address = b.integer(212);
        b.store(address, 0, reader, I32);
        let shift = b.op(O::I64Const { value: 32 }, &[], waffle::Type::I64);
        let writer = b.op(O::I64ShrU, &[pair, shift], waffle::Type::I64);
        b.op(O::I32WrapI64, &[writer], I32)
    };
    let thread = b.call(spawn, &[zero, input], &[I32])[0];
    b.call(resume, &[thread], &[I32]);
    let action = scheduler::host_action(&mut b, operations, HostTurn::Dispatch);
    b.ret(&[action]);
    b.finish(&mut module, entry)?;
    let callback = builder::declare(&mut module, "[callback][async-lift]run", &[I32; 3], &[I32]);
    let mut b = Builder::new(&module, callback, memory);
    let notify = b.body.add_block();
    let inspect = b.body.add_block();
    b.branch(b.param(0), notify, inspect);
    b.block = notify;
    b.call(
        operations.notify,
        &[b.param(0), b.param(1), b.param(2)],
        &[],
    );
    b.jump(inspect, &[]);
    b.block = inspect;
    let address = b.integer(204);
    let done = b.load(address, 0, I32);
    let finish = b.body.add_block();
    let wait = b.body.add_block();
    b.branch(done, finish, wait);
    b.block = wait;
    let action = scheduler::host_action(&mut b, operations, HostTurn::Dispatch);
    b.ret(&[action]);
    b.block = finish;
    b.call(operations.finish, &[], &[I32]);
    let address = b.integer(216);
    let result = b.load(address, 0, I32);
    b.call(native["[task-return]run"], &[result], &[]);
    b.call(allocator.post_return, &[], &[]);
    let zero = b.integer(0);
    b.ret(&[zero]);
    b.finish(&mut module, callback)?;
    for (name, function) in [
        ("[async-lift]run", entry),
        ("[callback][async-lift]run", callback),
    ] {
        module.exports.push(Export {
            name: name.into(),
            kind: ExportKind::Func(function),
        });
    }
    // Writable probes declare the payload type in an unused argument so canonical
    // transfer intrinsics bind to resolved application WIT rather than a custom ABI.
    let input = if probe.stream() {
        "stream<u8>"
    } else {
        "future<u32>"
    };
    let source = if probe.reading() {
        format!(
            "package test:transfers; world probe {{ export run:async func(mode:u32,input:{input})->u32; }}"
        )
    } else {
        format!(
            "package test:transfers; world probe {{ export run:async func(mode:u32)->u32; export payload:func(input:{input}); }}"
        )
    };
    if !probe.reading() {
        // Intrinsics use the payload-bearing export's resolved stream/future index.
        for import in &mut module.imports {
            if import.name.ends_with("-0]run") {
                import.name = import.name.replace("-0]run", "-0]payload");
            }
        }
        let payload = builder::declare(&mut module, "payload", &[I32], &[]);
        let mut b = Builder::new(&module, payload, memory);
        b.call(native[&drop_read], &[b.param(0)], &[]);
        b.ret(&[]);
        b.finish(&mut module, payload)?;
        module.exports.push(Export {
            name: "payload".into(),
            kind: ExportKind::Func(payload),
        });
    }
    let (resolve, package) = crate::component::wit::resolve_source(&source)?;
    let world = resolve.select_world(&[package], Some("probe"))?;
    crate::component::encode_resolved(&module.to_wasm_bytes()?, &resolve, world)
}

use crate::waffle_backend::test_input as input;
#[tokio::test(flavor = "current_thread")]
async fn transfers_share_owners_and_release_only_after_acknowledgement() -> Result<()> {
    use std::sync::{Arc, atomic::Ordering};
    use std::time::Duration;
    use wasmtime::component::{Component, FutureReader, Linker, StreamReader};
    use wasmtime::{Config, Engine, Store, StoreLimitsBuilder};
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    for probe in [
        Probe::StreamRead,
        Probe::StreamWrite,
        Probe::FutureRead,
        Probe::FutureWrite,
    ] {
        let component = Component::new(&engine, component(probe)?)?;
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(65536).build(),
        );
        store.limiter(|limits| limits);
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        if !probe.reading() {
            let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
            for _ in 0..100 {
                assert_eq!(run.call_async(&mut store, (0,)).await?.0, 2);
                store.assert_concurrent_state_empty();
            }
        } else if probe.stream() {
            let run =
                instance.get_typed_func::<(u32, StreamReader<u8>), (u32,)>(&mut store, "run")?;
            for _ in 0..100 {
                let observations = Arc::new(input::Observations::default());
                let (_sender, receiver) = tokio::sync::mpsc::channel(1);
                let stream = StreamReader::new(
                    &mut store,
                    input::ControlledProducer {
                        receiver,
                        observations: observations.clone(),
                    },
                )?;
                assert_eq!(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        run.call_async(&mut store, (0, stream))
                    )
                    .await??
                    .0,
                    2
                );
                assert!(observations.dropped.load(Ordering::SeqCst));
                store.assert_concurrent_state_empty();
            }
            let stream = StreamReader::new(&mut store, vec![42u8; 4 * 1024 * 1024])?;
            assert_eq!(
                run.call_async(&mut store, (1, stream)).await?.0,
                4 * 1024 * 1024
            );
            store.assert_concurrent_state_empty();
        } else {
            let run =
                instance.get_typed_func::<(u32, FutureReader<u32>), (u32,)>(&mut store, "run")?;
            for _ in 0..100 {
                let future =
                    FutureReader::new(&mut store, std::future::pending::<wasmtime::Result<u32>>())?;
                assert_eq!(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        run.call_async(&mut store, (0, future))
                    )
                    .await??
                    .0,
                    2
                );
                store.assert_concurrent_state_empty();
                let future = FutureReader::new(&mut store, async { wasmtime::error::Ok(42u32) })?;
                assert_eq!(run.call_async(&mut store, (1, future)).await?.0, 42);
                store.assert_concurrent_state_empty();
            }
        }
    }
    Ok(())
}
