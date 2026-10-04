use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;
use wasmtime::StoreContextMut;
use wasmtime::component::{FutureReader, Resource, StreamReader};
use wasmtime_wasi::p3::bindings::filesystem::types::{DescriptorType, DirectoryEntry, ErrorCode};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder, filesystem::Descriptor};

use super::super::{Host, instantiate_with};

use super::super::input::{ControlledStreamProducer, Observations};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    Failure,
    Trap,
    Dispose,
}

#[tokio::test(flavor = "current_thread")]
async fn directory_entries_and_completion_keep_owners_through_suspension_collection_and_disposal()
-> Result<()> {
    let source = r#"
    import {readdirSync} from 'fs';
    async function read(): Promise<string> {
        try {
            const names = readdirSync('/sandbox');
            let index = 0;
            while (index < 2000) { const temporary = new Uint8Array(256); index = index + 1; }
            if (names.length !== 81) { return 'wrong count'; }
            return names[0].slice(0) + names[80].slice(0);
        } catch (error) { if (error === 37) { return 'failed'; } throw error; }
        finally { console.log('finally'); }
    }
    export async function run(): Promise<string> {
        const pending = read();
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(256); index = index + 1; }
        console.log('collected');
        const first = await pending;
        index = 0;
        while (index < 2000) { const temporary = new Uint8Array(256); index = index + 1; }
        return first + await pending;
    }"#;
    for outcome in [
        Outcome::Success,
        Outcome::Failure,
        Outcome::Trap,
        Outcome::Dispose,
    ] {
        let directory = tempfile::tempdir()?;
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(8);
        sender
            .send(Ok([".", "..", "😀é"]
                .into_iter()
                .map(|name| DirectoryEntry {
                    type_: DescriptorType::Other(Some("kind 😀".repeat(50))),
                    name: name.into(),
                })
                .collect()))
            .await?;
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        let completion_pending = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let output = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(1024);
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
            .stdout(output.clone())
            .build();
        let (mut store, instance) = instantiate_with(source, context, |linker| {
            linker.allow_shadowing(true);
            let observations = observations.clone();
            let completion_pending = completion_pending.clone();
            let finish = finish.clone();
            linker.instance("wasi:filesystem/types@0.3.0")?.func_wrap(
                "[method]descriptor.read-directory",
                move |mut store: StoreContextMut<'_, Host>,
                      (descriptor,): (Resource<Descriptor>,)| {
                    store.data().table.get(&descriptor)?;
                    let stream = StreamReader::new(
                        &mut store,
                        ControlledStreamProducer {
                            receiver: receiver.lock().unwrap().take().unwrap(),
                            observations: observations.clone(),
                        },
                    )?;
                    let observations = observations.clone();
                    let completion_pending = completion_pending.clone();
                    let finish = finish.clone();
                    let completion = FutureReader::new(&mut store, async move {
                        observations.closed.notified().await;
                        completion_pending.notify_one();
                        finish.notified().await;
                        wasmtime::error::Ok(if outcome == Outcome::Failure {
                            Err(ErrorCode::Other(Some("late 😀".repeat(300))))
                        } else {
                            Ok(())
                        })
                    })?;
                    Ok(((stream, completion),))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        let mut invocation = Box::pin(run.call_async(&mut store, ()));
        tokio::select! {
            result = &mut invocation => panic!("completed before entries: {result:?}"),
            () = observations.pending.notified() => {}
        }
        tokio::select! {
            result = &mut invocation => panic!("completed before collection: {result:?}"),
            result = timeout(Duration::from_secs(5), async { while output.contents().is_empty() { tokio::task::yield_now().await; } }) => { result?; }
        }
        assert_eq!(output.contents(), b"collected\n"[..]);
        assert_eq!(observations.bytes.load(Ordering::SeqCst), 3);
        match outcome {
            Outcome::Dispose => {
                drop(invocation);
                drop(store);
                assert!(sender.is_closed());
            }
            Outcome::Trap => {
                sender.send(Err("controlled directory trap".into())).await?;
                let error = timeout(Duration::from_secs(5), invocation)
                    .await?
                    .unwrap_err();
                assert!(format!("{error:#}").contains("controlled directory trap"));
                drop(store);
                assert!(sender.is_closed());
                assert_eq!(output.contents(), b"collected\n"[..]);
            }
            Outcome::Success | Outcome::Failure => {
                sender.send(Ok(vec![])).await?;
                sender
                    .send(Ok((0..80)
                        .map(|index| DirectoryEntry {
                            type_: DescriptorType::RegularFile,
                            name: format!("entry{index}中"),
                        })
                        .collect()))
                    .await?;
                drop(sender);
                tokio::select! {
                    result = &mut invocation => panic!("returned at EOF before completion: {result:?}"),
                    () = completion_pending.notified() => {}
                }
                assert!(
                    timeout(Duration::from_millis(1), &mut invocation)
                        .await
                        .is_err()
                );
                finish.notify_one();
                let result = timeout(Duration::from_secs(5), invocation).await??.0;
                assert_eq!(
                    result,
                    if outcome == Outcome::Success {
                        "😀éentry79中😀éentry79中"
                    } else {
                        "failedfailed"
                    }
                );
                assert_eq!(output.contents(), b"collected\nfinally\n"[..]);
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
        assert!(observations.dropped.load(Ordering::SeqCst));
    }
    Ok(())
}
