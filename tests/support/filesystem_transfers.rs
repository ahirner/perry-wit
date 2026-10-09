//! Deterministic native transfers for partial I/O, delayed failures and cancellation.
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use wasmtime::{
    StoreContextMut,
    component::{
        Destination, FutureProducer, FutureReader, Linker, Resource, Source, StreamConsumer,
        StreamProducer, StreamReader, StreamResult, VecBuffer,
    },
};
use wasmtime_wasi::{
    filesystem::Descriptor,
    p3::bindings::filesystem::types::{
        DescriptorFlags, DescriptorType, DirectoryEntry, ErrorCode, OpenFlags, PathFlags,
    },
};

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Read,
    Write,
    Directory,
}
#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    Success,
    FinishError,
    BlockData,
    BlockCompletion,
    Failure,
    EarlyClose,
    EarlyFailure,
}
#[derive(Default)]
pub struct Observations {
    pub ready: AtomicBool,
    pub started: AtomicUsize,
    pub cancelled: AtomicUsize,
    pub dropped_data: AtomicUsize,
    pub dropped_future: AtomicUsize,
    pub transferred: AtomicUsize,
}
struct Producer<Item> {
    items: Option<Vec<Item>>,
    behavior: Behavior,
    observations: Arc<Observations>,
    descriptor: u32,
}
impl<Item: Send + Sync + Unpin + 'static> StreamProducer<super::Host> for Producer<Item> {
    type Item = Item;
    type Buffer = VecBuffer<Item>;
    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        store: StoreContextMut<'a, super::Host>,
        mut destination: Destination<'a, Item, VecBuffer<Item>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        store
            .data()
            .table
            .get(&Resource::<Descriptor>::new_borrow(self.descriptor))?;
        if finish {
            self.observations.cancelled.fetch_add(1, Ordering::SeqCst);
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if matches!(self.behavior, Behavior::BlockData) {
            self.observations.ready.store(true, Ordering::SeqCst);
            return Poll::Pending;
        }
        match self.items.take() {
            Some(items) => {
                self.observations
                    .transferred
                    .fetch_add(items.len(), Ordering::SeqCst);
                destination.set_buffer(items.into());
                Poll::Ready(Ok(StreamResult::Completed))
            }
            None => Poll::Ready(Ok(StreamResult::Dropped)),
        }
    }
}
impl<Item> Drop for Producer<Item> {
    fn drop(&mut self) {
        self.observations
            .dropped_data
            .fetch_add(1, Ordering::SeqCst);
    }
}
struct Consumer {
    behavior: Behavior,
    observations: Arc<Observations>,
    descriptor: u32,
}
impl StreamConsumer<super::Host> for Consumer {
    type Item = u8;
    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        store: StoreContextMut<'_, super::Host>,
        source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        store
            .data()
            .table
            .get(&Resource::<Descriptor>::new_borrow(self.descriptor))?;
        if finish {
            self.observations.cancelled.fetch_add(1, Ordering::SeqCst);
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if matches!(self.behavior, Behavior::BlockData) {
            self.observations.ready.store(true, Ordering::SeqCst);
            return Poll::Pending;
        }
        if matches!(self.behavior, Behavior::EarlyClose | Behavior::EarlyFailure) {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        let mut source = source.as_direct(store);
        let count = source.remaining().len().min(3);
        source.mark_read(count);
        self.observations
            .transferred
            .fetch_add(count, Ordering::SeqCst);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        self.observations
            .dropped_data
            .fetch_add(1, Ordering::SeqCst);
    }
}
struct Completion {
    behavior: Behavior,
    observations: Arc<Observations>,
    descriptor: u32,
}
impl FutureProducer<super::Host> for Completion {
    type Item = Result<(), ErrorCode>;
    fn poll_produce(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'_, super::Host>,
        finish: bool,
    ) -> Poll<wasmtime::Result<Option<Self::Item>>> {
        store
            .data()
            .table
            .get(&Resource::<Descriptor>::new_borrow(self.descriptor))?;
        if finish {
            self.observations.cancelled.fetch_add(1, Ordering::SeqCst);
            return Poll::Ready(Ok(if matches!(self.behavior, Behavior::FinishError) {
                Some(Err(ErrorCode::Other(Some(
                    "cancelled completion é🙂".repeat(8192),
                ))))
            } else {
                None
            }));
        }
        if matches!(
            self.behavior,
            Behavior::BlockData | Behavior::BlockCompletion | Behavior::FinishError
        ) {
            self.observations.ready.store(true, Ordering::SeqCst);
            return Poll::Pending;
        }
        if self.observations.dropped_data.load(Ordering::SeqCst)
            < self.observations.started.load(Ordering::SeqCst)
        {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(Ok(Some(
            if matches!(self.behavior, Behavior::Failure | Behavior::EarlyFailure) {
                Err(ErrorCode::Other(Some("late é🙂".repeat(8192))))
            } else {
                Ok(())
            },
        )))
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        self.observations
            .dropped_future
            .fetch_add(1, Ordering::SeqCst);
    }
}

pub fn install(
    linker: &mut Linker<super::Host>,
    kind: Kind,
    behavior: Behavior,
) -> anyhow::Result<Arc<Observations>> {
    linker.allow_shadowing(true);
    linker
        .instance("wasi:filesystem/types@0.3.0")?
        .func_wrap_concurrent(
            "[method]descriptor.open-at",
            |accessor,
             (parent, flags, _, _, _): (
                Resource<Descriptor>,
                PathFlags,
                String,
                OpenFlags,
                DescriptorFlags,
            )| {
                Box::pin(async move {
                    assert_eq!(flags, PathFlags::SYMLINK_FOLLOW);
                    let result = accessor.with(|mut access| {
                        let descriptor = access.data_mut().table.get(&parent)?.clone();
                        access.data_mut().table.push(descriptor)
                    });
                    Ok((Ok::<_, ErrorCode>(result?),))
                })
            },
        )?;
    let observations = Arc::new(Observations::default());
    let tracked = observations.clone();
    let mut interface = linker.instance("wasi:filesystem/types@0.3.0")?;
    match kind {
        Kind::Write => {
            interface.func_wrap(
                "[method]descriptor.write-via-stream",
                move |mut store: StoreContextMut<'_, super::Host>,
                      (descriptor, input, offset): (
                    Resource<Descriptor>,
                    StreamReader<u8>,
                    u64,
                )| {
                    assert_eq!(offset, 0);
                    store.data().table.get(&descriptor)?;
                    tracked.started.fetch_add(1, Ordering::SeqCst);
                    input.pipe(
                        &mut store,
                        Consumer {
                            behavior,
                            observations: tracked.clone(),
                            descriptor: descriptor.rep(),
                        },
                    )?;
                    Ok((FutureReader::new(
                        &mut store,
                        Completion {
                            behavior,
                            observations: tracked.clone(),
                            descriptor: descriptor.rep(),
                        },
                    )?,))
                },
            )?;
        }
        Kind::Read => {
            interface.func_wrap(
                "[method]descriptor.read-via-stream",
                move |mut store: StoreContextMut<'_, super::Host>,
                      (descriptor, offset): (Resource<Descriptor>, u64)| {
                    assert_eq!(offset, 0);
                    store.data().table.get(&descriptor)?;
                    tracked.started.fetch_add(1, Ordering::SeqCst);
                    let stream = StreamReader::new(
                        &mut store,
                        Producer {
                            items: Some("\u{feff}é🙂".repeat(3000).into_bytes()),
                            behavior,
                            observations: tracked.clone(),
                            descriptor: descriptor.rep(),
                        },
                    )?;
                    let future = FutureReader::new(
                        &mut store,
                        Completion {
                            behavior,
                            observations: tracked.clone(),
                            descriptor: descriptor.rep(),
                        },
                    )?;
                    Ok(((stream, future),))
                },
            )?;
        }
        Kind::Directory => {
            interface.func_wrap(
                "[method]descriptor.read-directory",
                move |mut store: StoreContextMut<'_, super::Host>,
                      (descriptor,): (Resource<Descriptor>,)| {
                    store.data().table.get(&descriptor)?;
                    tracked.started.fetch_add(1, Ordering::SeqCst);
                    let entries = (-2..36)
                        .map(|i| DirectoryEntry {
                            type_: DescriptorType::Other(Some("kindé".repeat(4096))),
                            name: if i == -2 {
                                ".".into()
                            } else if i == -1 {
                                "..".into()
                            } else {
                                format!("entry-{i}🙂")
                            },
                        })
                        .collect();
                    let stream = StreamReader::new(
                        &mut store,
                        Producer {
                            items: Some(entries),
                            behavior,
                            observations: tracked.clone(),
                            descriptor: descriptor.rep(),
                        },
                    )?;
                    let future = FutureReader::new(
                        &mut store,
                        Completion {
                            behavior,
                            observations: tracked.clone(),
                            descriptor: descriptor.rep(),
                        },
                    )?;
                    Ok(((stream, future),))
                },
            )?;
        }
    }
    let readiness = observations.clone();
    linker.instance("test:control/cancellation")?.func_wrap(
        "ready",
        move |_: StoreContextMut<'_, super::Host>, (): ()| {
            Ok((readiness.ready.load(Ordering::SeqCst),))
        },
    )?;
    Ok(observations)
}
