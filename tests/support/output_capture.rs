//! Bounded test-owned output capture using the host's async byte interface.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

#[derive(Clone)]
pub(crate) struct MemoryOutput {
    bytes: Arc<Mutex<Vec<u8>>>,
    limit: usize,
}

impl MemoryOutput {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Arc::default(),
            limit,
        }
    }

    pub(crate) fn contents(&self) -> bytes::Bytes {
        bytes::Bytes::copy_from_slice(&self.bytes.lock().unwrap())
    }
}

impl IsTerminal for MemoryOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for MemoryOutput {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl AsyncWrite for MemoryOutput {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut bytes = self.bytes.lock().unwrap();
        let count = data.len().min(self.limit - bytes.len());
        if count == 0 && !data.is_empty() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "test output limit reached",
            )));
        }
        bytes.extend_from_slice(&data[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
