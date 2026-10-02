//! Standard output, error, and process failure handlers using WASI CLI.

use crate::bindings::wasi::cli::exit::exit;
use crate::bindings::wasi::cli::stderr::get_stderr;
use crate::bindings::wasi::cli::stdout::get_stdout;

pub(crate) fn print_stdout(text: &str) {
    let stdout = get_stdout();
    for chunk in text.as_bytes().chunks(4096) {
        let _ = stdout.blocking_write_and_flush(chunk);
    }
}

pub(crate) fn print_stderr(text: &str) {
    let stderr = get_stderr();
    for chunk in text.as_bytes().chunks(4096) {
        let _ = stderr.blocking_write_and_flush(chunk);
    }
}

pub(crate) fn fail_with_error(msg: &str) -> ! {
    print_stderr(&format!("Error: {msg}\n"));
    exit(Err(()));
    core::arch::wasm32::unreachable();
}
