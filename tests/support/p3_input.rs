use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use tokio::sync::{Notify, mpsc};
use wasmtime::StoreContextMut;
use wasmtime::component::{Destination, StreamProducer, StreamResult, VecBuffer};

#[derive(Default)]
pub(crate) struct Observations {
    pub(crate) bytes: AtomicUsize,
    pub(crate) max_request: AtomicUsize,
    pub(crate) dropped: AtomicBool,
    pub(crate) pending: Notify,
    pub(crate) closed: Notify,
}

pub(crate) struct ControlledProducer {
    pub(crate) receiver: mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
    pub(crate) observations: Arc<Observations>,
}

impl<T> StreamProducer<T> for ControlledProducer {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, T>,
        mut destination: Destination<'a, u8, VecBuffer<u8>>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        self.observations
            .max_request
            .fetch_max(destination.remaining(&mut store).unwrap(), Ordering::SeqCst);
        loop {
            match self.receiver.poll_recv(cx) {
                Poll::Pending => {
                    self.observations.pending.notify_one();
                    return Poll::Pending;
                }
                // Nonzero native reads need data or Pending; an empty application chunk is neither EOF nor a completed transfer.
                Poll::Ready(Some(Ok(bytes))) if bytes.is_empty() => continue,
                Poll::Ready(Some(Ok(bytes))) => {
                    self.observations
                        .bytes
                        .fetch_add(bytes.len(), Ordering::SeqCst);
                    destination.set_buffer(bytes.into());
                    return Poll::Ready(Ok(StreamResult::Completed));
                }
                Poll::Ready(Some(Err(message))) => {
                    return Poll::Ready(Err(wasmtime::Error::msg(message)));
                }
                Poll::Ready(None) => return Poll::Ready(Ok(StreamResult::Dropped)),
            }
        }
    }
}

impl Drop for ControlledProducer {
    fn drop(&mut self) {
        self.observations.dropped.store(true, Ordering::SeqCst);
        self.observations.closed.notify_one();
    }
}
