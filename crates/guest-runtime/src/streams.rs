//! Bounded stream forwarding with explicit readiness and flush boundaries.

use crate::bindings::wasi::io::poll::Pollable;
use crate::bindings::wasi::io::streams::{InputStream, OutputStream, StreamError};

const CHUNK_SIZE: u64 = 8192;

/// Holds at most one chunk and flushes it before requesting more input.
pub(crate) fn forward(
    input: &InputStream,
    output: &OutputStream,
    mut wait: impl FnMut(Pollable) -> Result<(), String>,
) -> Result<(), String> {
    loop {
        match input.read(CHUNK_SIZE) {
            Ok(bytes) if bytes.is_empty() => wait(input.subscribe())?,
            Ok(bytes) => write(output, &bytes, &mut wait)?,
            Err(StreamError::Closed) => return write(output, &[], wait),
            Err(error) => return Err(stream_error(error)),
        }
    }
}

/// Respects each write permit, including partial capacity and flush completion.
pub(crate) fn write(
    output: &OutputStream,
    bytes: &[u8],
    mut wait: impl FnMut(Pollable) -> Result<(), String>,
) -> Result<(), String> {
    let mut offset = 0;
    while offset < bytes.len() {
        let capacity = output.check_write().map_err(stream_error)?;
        let count = capacity.min(CHUNK_SIZE).min((bytes.len() - offset) as u64) as usize;
        if count == 0 {
            wait(output.subscribe())?;
        } else {
            output
                .write(&bytes[offset..offset + count])
                .map_err(stream_error)?;
            offset += count;
        }
    }
    output.flush().map_err(stream_error)?;
    while output.check_write().map_err(stream_error)? == 0 {
        wait(output.subscribe())?;
    }
    Ok(())
}

/// Formats and releases an owned host error before its stream can be dropped.
fn stream_error(error: StreamError) -> String {
    format!("HTTP body stream failed: {error:?}")
}
