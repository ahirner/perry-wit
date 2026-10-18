//! Native filesystem operations retain their arguments and result storage across callbacks.
use anyhow::Result;
use perry_wit::{compiler::compile_for_world_with_memory_checks, component::wit::resolve_wit};
use wasmtime::component::Component;
use wasmtime_wasi::{FsPerms, WasiCtx};

use std::sync::atomic::Ordering;
#[path = "../support/cancellable_output.rs"]
mod cancellable_output;
use crate::cancellation;
use crate::engine::async_engine;
use crate::wasi_host::{self, Host};
#[path = "../support/filesystem_transfers.rs"]
mod transfers;

fn compile(source: &str, exports: &str) -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        format!(
            "package test:filesystem; world filesystem {{ import wasi:filesystem/types@0.3.0; import wasi:filesystem/preopens@0.3.0; import wasi:cli/stderr@0.3.0; {exports} }}"
        ),
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("filesystem"))?;
    let compiled = compile_for_world_with_memory_checks(source, "filesystem.ts", &resolve, world)?;
    Ok(compiled.component.unwrap().stripped)
}

#[tokio::test(flavor = "current_thread")]
async fn transfer_cancellation_and_late_errors_release_every_native_owner() -> Result<()> {
    use transfers::{Behavior, Kind};
    let engine = async_engine()?;
    for (kind, call) in [
        (Kind::Read, "readFile('/sandbox/input')"),
        (
            Kind::Write,
            "writeFile('/sandbox/output','partial é🙂 write')",
        ),
        (Kind::Directory, "readdir('/sandbox')"),
    ] {
        for behavior in [
            Behavior::Success,
            Behavior::BlockData,
            Behavior::PartialData,
            Behavior::BlockCompletion,
            Behavior::FinishError,
            Behavior::Failure,
            Behavior::EarlyClose,
            Behavior::EarlyFailure,
        ] {
            if matches!(behavior, Behavior::EarlyClose | Behavior::EarlyFailure)
                && !matches!(kind, Kind::Write)
            {
                continue;
            }
            let cancel = matches!(
                behavior,
                Behavior::BlockData
                    | Behavior::PartialData
                    | Behavior::BlockCompletion
                    | Behavior::FinishError
            );
            let body = if matches!(behavior, Behavior::Success) {
                match kind {
                    Kind::Read => {
                        format!("const value=await {call};return value.length===27000?1:0;")
                    }
                    Kind::Directory => {
                        format!("const value=await {call};return value.length===36?1:0;")
                    }
                    Kind::Write => format!("await {call};return 1;"),
                }
            } else if cancel {
                format!(
                    "try{{await {call};}}finally{{throw new Error('cancelled continuation ran');}}"
                )
            } else {
                format!(
                    "try{{await {call};}}catch(e){{if(e.message.startsWith('EIO:'))return 1;throw e;}}return 0;"
                )
            };
            let source = format!(
                "import {{readFile,writeFile,readdir}} from 'node:fs/promises';export async function run():Promise<{}>{{{body}}}",
                if cancel { "void" } else { "number" }
            );
            let worker = compile(
                &source,
                if cancel {
                    "export run: async func();"
                } else {
                    "export run: async func() -> f64;"
                },
            )?;
            let component = Component::new(
                &engine,
                if cancel {
                    cancellation::cancellation_caller(&worker, true)?
                } else {
                    worker
                },
            )?;
            let root = tempfile::tempdir()?;
            let ctx = WasiCtx::builder()
                .preopened_dir(root.path(), "/sandbox", FsPerms::ReadWrite)?
                .build();
            let mut store = wasi_host::store(&engine, Host::new(ctx));
            let mut linker = wasi_host::linker(&engine)?;
            let observations = transfers::install(&mut linker, kind, behavior)?;
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let mut peak = 0;
            for round in 0..8 {
                observations.ready.store(false, Ordering::SeqCst);
                if cancel {
                    let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
                    assert_eq!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            run.call_async(&mut store, ())
                        )
                        .await??
                        .0,
                        4,
                        "{kind:?} {behavior:?}"
                    );
                    assert!(
                        observations.cancelled.load(Ordering::SeqCst) > round,
                        "cancellation missed transfer phase: {kind:?} {behavior:?}"
                    );
                } else {
                    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
                    assert_eq!(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            run.call_async(&mut store, ())
                        )
                        .await??
                        .0,
                        1.0,
                        "{kind:?} {behavior:?}"
                    );
                }
                assert_eq!(
                    observations.started.load(Ordering::SeqCst),
                    round + 1,
                    "open cancellation hid transfer test"
                );
                assert_eq!(observations.dropped_data.load(Ordering::SeqCst), round + 1);
                if matches!(behavior, Behavior::Success) {
                    let count = match kind {
                        Kind::Read => 27000,
                        Kind::Directory => 38,
                        Kind::Write => "partial é🙂 write".len(),
                    };
                    assert_eq!(
                        observations.transferred.load(Ordering::SeqCst),
                        (round + 1) * count
                    );
                }
                if matches!(behavior, Behavior::PartialData) {
                    assert_eq!(
                        observations.transferred.load(Ordering::SeqCst),
                        (round + 1) * 3
                    );
                }
                assert_eq!(
                    observations.dropped_future.load(Ordering::SeqCst),
                    round + 1
                );
                assert!(store.data().table.is_empty());
                store.assert_concurrent_state_empty();
                if round == 1 {
                    peak = store.data().peak;
                } else if round > 1 {
                    assert_eq!(store.data().peak, peak, "{kind:?} {behavior:?}");
                }
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn caller_cancellation_releases_output_buffers_and_completion_futures() -> Result<()> {
    use cancellable_output::BlockAt;
    use std::sync::atomic::Ordering;
    let engine = async_engine()?;
    for (channel, method) in [("stdout", "log"), ("stderr", "error")] {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("world.wit"),
            "package test:cancel; world worker { import wasi:cli/stdout@0.3.0; import wasi:cli/stderr@0.3.0; export run: async func(); }",
        )?;
        let (resolve, package) = resolve_wit(directory.path())?;
        let world = resolve.select_world(&[package], Some("worker"))?;
        let source = format!(
            "export async function run(): Promise<void> {{ console.{method}('{}'); console.{method}('queued'); }}",
            "é🙂".repeat(20000)
        );
        let worker =
            compile_for_world_with_memory_checks(&source, "cancel-output.ts", &resolve, world)?
                .component
                .unwrap()
                .stripped;
        let caller = cancellation::cancellation_caller(&worker, false)?;
        wasmparser::Validator::new().validate_all(&caller)?;
        let component = Component::new(&engine, caller)?;
        for block_at in [BlockAt::Write, BlockAt::Completion] {
            let mut linker = wasi_host::linker(&engine)?;
            let observations = cancellable_output::install(
                &mut linker,
                &format!("wasi:cli/{channel}@0.3.0"),
                block_at,
            )?;
            let mut store = wasi_host::store(&engine, Host::default());
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
            let mut peak = 0;
            for round in 1..=10 {
                let (status,) = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    run.call_async(&mut store, ()),
                )
                .await??;
                assert_eq!(status, 4, "{channel} {block_at:?}");
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
                assert_eq!(
                    observations.started.load(Ordering::SeqCst),
                    round,
                    "queued output started during cancellation"
                );
                assert_eq!(observations.dropped_writes.load(Ordering::SeqCst), round);
                assert_eq!(
                    observations.dropped_completions.load(Ordering::SeqCst),
                    round
                );
                let cancelled = match block_at {
                    BlockAt::Write => &observations.cancelled_writes,
                    BlockAt::Completion => &observations.cancelled_completions,
                };
                assert_eq!(
                    cancelled.load(Ordering::SeqCst),
                    round,
                    "pending operation was not cancelled"
                );
                if round == 1 {
                    peak = store.data().peak;
                } else {
                    assert_eq!(
                        store.data().peak,
                        peak,
                        "cancelled output memory was retained"
                    );
                }
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_results_release_unused_payloads_and_cancelled_borrows() -> Result<()> {
    use std::sync::{Arc, atomic::AtomicUsize};
    use wasmtime::component::Resource;
    use wasmtime_wasi::{
        filesystem::Descriptor,
        p3::bindings::filesystem::types::{DescriptorStat, DescriptorType, ErrorCode, PathFlags},
    };
    struct Pending(Arc<AtomicUsize>);
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let engine = async_engine()?;
    for cancelled in [false, true] {
        let body = if cancelled {
            "stat('/sandbox/first');try{await stat('/sandbox/second');}finally{throw new Error('cancelled continuation ran');}"
        } else {
            "let failed=false;try{await stat('/sandbox/error');}catch(error){failed=error.message.startsWith('EIO:');}const value=await stat('/sandbox/other');return failed && value.size>0 && !value.isFile() && !value.isDirectory() ? 1 : 0;"
        };
        let source = format!(
            "import {{stat}} from 'node:fs/promises';export async function run():Promise<{}>{{{body}}}",
            if cancelled { "void" } else { "number" }
        );
        let worker = compile(
            &source,
            if cancelled {
                "export run:async func();"
            } else {
                "export run:async func()->f64;"
            },
        )?;
        let component = Component::new(
            &engine,
            if cancelled {
                cancellation::cancellation_caller(&worker, false)?
            } else {
                worker
            },
        )?;
        let active = Arc::new(AtomicUsize::new(0));
        let tracked = active.clone();
        let mut linker = wasi_host::linker(&engine)?;
        linker.allow_shadowing(true);
        linker
            .instance("wasi:filesystem/types@0.3.0")?
            .func_wrap_concurrent(
                "[method]descriptor.stat-at",
                move |_, (_, flags, path): (Resource<Descriptor>, PathFlags, String)| {
                    assert_eq!(flags, PathFlags::SYMLINK_FOLLOW);
                    tracked.fetch_add(1, Ordering::SeqCst);
                    let pending = Pending(tracked.clone());
                    Box::pin(async move {
                        let _pending = pending;
                        if cancelled {
                            std::future::pending::<()>().await;
                        }
                        tokio::task::yield_now().await;
                        let other = Some("é🙂".repeat(32768));
                        Ok((if path.ends_with("error") {
                            Err(ErrorCode::Other(other))
                        } else {
                            Ok(DescriptorStat {
                                type_: DescriptorType::Other(other),
                                link_count: 17,
                                size: u64::MAX,
                                data_access_timestamp: None,
                                data_modification_timestamp: None,
                                status_change_timestamp: None,
                            })
                        },))
                    })
                },
            )?;
        let root = tempfile::tempdir()?;
        let ctx = WasiCtx::builder()
            .preopened_dir(root.path(), "/sandbox", FsPerms::ReadWrite)?
            .build();
        let mut store = wasi_host::store(&engine, Host::new(ctx));
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let mut peak = 0;
        for round in 0..12 {
            if cancelled {
                let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
                assert_eq!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        run.call_async(&mut store, ())
                    )
                    .await??
                    .0,
                    4
                );
            } else {
                let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
                assert_eq!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        run.call_async(&mut store, ())
                    )
                    .await??
                    .0,
                    1.0
                );
            }
            assert_eq!(active.load(Ordering::SeqCst), 0);
            assert!(store.data().table.is_empty());
            store.assert_concurrent_state_empty();
            if round == 1 {
                peak = store.data().peak;
            } else if round > 1 {
                assert_eq!(store.data().peak, peak, "cancelled={cancelled}");
            }
        }
    }
    Ok(())
}
