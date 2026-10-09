//! Deterministic P3 output hosts that block either the write or its completion future.
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use wasmtime::{
    StoreContextMut,
    component::{FutureProducer, FutureReader, Source, StreamConsumer, StreamReader, StreamResult},
};
use wasmtime_wasi::p3::bindings::cli::types::ErrorCode;

#[derive(Clone, Copy, Debug)]
pub enum BlockAt {
    Write,
    Completion,
}

#[derive(Default)]
pub struct Observations {
    pub started: AtomicUsize,
    pub cancelled_writes: AtomicUsize,
    pub cancelled_completions: AtomicUsize,
    pub dropped_writes: AtomicUsize,
    pub dropped_completions: AtomicUsize,
}

struct Output {
    block_at: BlockAt,
    observations: Arc<Observations>,
}

impl StreamConsumer<super::Host> for Output {
    type Item = u8;
    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        store: StoreContextMut<'_, super::Host>,
        source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        match self.block_at {
            BlockAt::Write if finish => {
                self.observations
                    .cancelled_writes
                    .fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Ok(StreamResult::Cancelled))
            }
            BlockAt::Write => Poll::Pending,
            BlockAt::Completion => {
                let mut source = source.as_direct(store);
                let count = source.remaining().len();
                source.mark_read(count);
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.observations
            .dropped_writes
            .fetch_add(1, Ordering::SeqCst);
    }
}

struct Completion(Arc<Observations>);
impl FutureProducer<super::Host> for Completion {
    type Item = Result<(), ErrorCode>;
    fn poll_produce(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: StoreContextMut<'_, super::Host>,
        finish: bool,
    ) -> Poll<wasmtime::Result<Option<Self::Item>>> {
        if finish {
            self.0.cancelled_completions.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(None))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        self.0.dropped_completions.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn install(
    linker: &mut wasmtime::component::Linker<super::Host>,
    interface: &str,
    block_at: BlockAt,
) -> anyhow::Result<Arc<Observations>> {
    let observations = Arc::new(Observations::default());
    let output_observations = observations.clone();
    linker.allow_shadowing(true);
    linker.instance(interface)?.func_wrap(
        "write-via-stream",
        move |mut store: StoreContextMut<'_, super::Host>, (reader,): (StreamReader<u8>,)| {
            output_observations.started.fetch_add(1, Ordering::SeqCst);
            reader.pipe(
                &mut store,
                Output {
                    block_at,
                    observations: output_observations.clone(),
                },
            )?;
            Ok((FutureReader::new(
                &mut store,
                Completion(output_observations.clone()),
            )?,))
        },
    )?;
    Ok(observations)
}
