//! The body protocol shared by HTTP clients and handlers.
use crate::{
    alloc,
    headers::wasi::fields_drop,
    readable::{Readable, body_stream, complete},
};
use core::marker::PhantomData;

/// Canonical result areas have their payload at byte 8.
pub(crate) const RESULT_SIZE: u32 = 40;

macro_rules! canonical_signatures {
    ($abi:literal) => {
        pub(crate) type WriteContents = unsafe extern $abi fn(u32, *const u8, u32) -> u32;
        pub(crate) type WriteFuture = unsafe extern $abi fn(u32, *const u8) -> u32;
        pub(crate) type ReadFuture = unsafe extern $abi fn(u32, *mut u8) -> u32;
        pub(crate) type NewFuture = unsafe extern $abi fn() -> u64;
        pub(crate) type ConsumeBody = unsafe extern $abi fn(u32, u32, *mut u32);
        pub(crate) type Cancel = unsafe extern $abi fn(u32) -> u32;
        pub(crate) type DropHandle = unsafe extern $abi fn(u32);
    };
}
#[cfg(target_arch = "wasm32")]
canonical_signatures!("C");
#[cfg(not(target_arch = "wasm32"))]
canonical_signatures!("Rust");

macro_rules! endpoints {
    ($($name:ident: $signature:ident($($arg:ident: $ty:ty),*) $(-> $result:ty)?
        = $prefix:literal $direction:ident $method:literal;)*) => {
        pub(crate) trait Endpoint {
            const OUTGOING_REQUEST: bool;
            $(const $name: $signature;)*
        }
        endpoints!(@policy Request, true, "request", "response";
            $($name: $signature($($arg: $ty),*) $(-> $result)? = $prefix $direction $method;)*);
        endpoints!(@policy Response, false, "response", "request";
            $($name: $signature($($arg: $ty),*) $(-> $result)? = $prefix $direction $method;)*);
    };
    (@policy $policy:ident, $request:literal, $outgoing:literal, $incoming:literal;
        $($name:ident: $signature:ident($($arg:ident: $ty:ty),*) $(-> $result:ty)?
            = $prefix:literal $direction:ident $method:literal;)*) => {
        pub(crate) struct $policy;
        impl Endpoint for $policy {
            const OUTGOING_REQUEST: bool = $request;
            $(const $name: $signature = {
                perry_abi::wasm_imports! { "wasi:http/types@0.3.0";
                    #[link_name = concat!($prefix, endpoints!(@direction $direction, $outgoing, $incoming), $method)]
                    fn call($($arg: $ty),*) $(-> $result)?;
                }
                call
            };)*
        }
    };
    (@direction outgoing, $outgoing:literal, $incoming:literal) => { $outgoing };
    (@direction incoming, $outgoing:literal, $incoming:literal) => { $incoming };
}

endpoints! {
    OUTCOME_NEW: NewFuture() -> u64 = "[future-new-0][static]" incoming ".consume-body";
    CONSUME_BODY: ConsumeBody(message: u32, outcome: u32, result: *mut u32)
        = "[static]" incoming ".consume-body";
    CONTENTS_WRITE: WriteContents(stream: u32, data: *const u8, len: u32) -> u32
        = "[async-lower][stream-write-0][static]" outgoing ".new";
    CONTENTS_CANCEL: Cancel(stream: u32) -> u32 = "[stream-cancel-write-0][static]" outgoing ".new";
    CONTENTS_DROP: DropHandle(stream: u32) = "[stream-drop-writable-0][static]" outgoing ".new";
    TRAILERS_WRITE: WriteFuture(future: u32, value: *const u8) -> u32
        = "[async-lower][future-write-1][static]" outgoing ".new";
    TRAILERS_CANCEL: Cancel(future: u32) -> u32 = "[future-cancel-write-1][static]" outgoing ".new";
    TRAILERS_DROP: DropHandle(future: u32) = "[future-drop-writable-1][static]" outgoing ".new";
    OUTCOME_WRITE: WriteFuture(future: u32, value: *const u8) -> u32
        = "[async-lower][future-write-0][static]" incoming ".consume-body";
    OUTCOME_CANCEL: Cancel(future: u32) -> u32 = "[future-cancel-write-0][static]" incoming ".consume-body";
    OUTCOME_DROP: DropHandle(future: u32) = "[future-drop-writable-0][static]" incoming ".consume-body";
    END_READ: ReadFuture(future: u32, result: *mut u8) -> u32
        = "[async-lower][future-read-2][static]" incoming ".consume-body";
    END_CANCEL: Cancel(future: u32) -> u32 = "[future-cancel-read-2][static]" incoming ".consume-body";
    END_DROP: DropHandle(future: u32) = "[future-drop-readable-2][static]" incoming ".consume-body";
    TRANSMIT_READ: ReadFuture(future: u32, result: *mut u8) -> u32
        = "[async-lower][future-read-2][static]" outgoing ".new";
    TRANSMIT_CANCEL: Cancel(future: u32) -> u32 = "[future-cancel-read-2][static]" outgoing ".new";
    TRANSMIT_DROP: DropHandle(future: u32) = "[future-drop-readable-2][static]" outgoing ".new";
}

#[derive(Default)]
struct Waitable {
    handle: u32,
    blocked: bool,
}

impl Waitable {
    fn new(handle: u32) -> Self {
        Self {
            handle,
            blocked: false,
        }
    }

    fn wait(&mut self, code: u32) -> bool {
        self.blocked = code == perry_abi::BLOCKED;
        if self.blocked {
            perry_abi::join(self.handle);
        }
        self.blocked
    }

    fn write(&mut self, value: *const u8, write: WriteFuture, drop: DropHandle) {
        if self.handle != 0 {
            // SAFETY: the exchange retains the value until completion or cancellation acknowledgement.
            let code = unsafe { write(self.handle, value) };
            if !self.wait(code) {
                self.finish(drop);
            }
        }
    }

    fn read(&mut self, result: *mut u8, read: ReadFuture) -> bool {
        // SAFETY: the exchange owns this readable end and retains the result area.
        let code = unsafe { read(self.handle, result) };
        !self.wait(code)
    }

    fn finish(&mut self, drop: DropHandle) {
        perry_abi::unjoin(self.handle);
        self.release(drop);
    }

    fn cancel(&mut self, cancel: Cancel, drop: DropHandle) {
        if self.handle != 0 {
            perry_abi::unjoin(self.handle);
            if self.blocked {
                // SAFETY: acknowledge cancellation before releasing the handle or its storage.
                unsafe { perry_abi::acknowledged(|| cancel(self.handle)) };
            }
            self.release(drop);
        }
    }

    fn release(&mut self, drop: DropHandle) {
        // SAFETY: the exchange owns this idle, unjoined handle.
        unsafe { drop(self.handle) };
        *self = Self::default();
    }
}

/// Handles are zero when absent. The containing pinned operation traces all five pointers.
pub(crate) struct Exchange<E> {
    contents: Waitable,
    pub(crate) data: *const u8,
    left: u32,
    trailers: Waitable,
    end: Waitable,
    outcome: Waitable,
    pub(crate) body: *mut Readable,
    transmit: Waitable,
    pub(crate) ended: *mut u8,
    pub(crate) transmitted: *mut u8,
    pub(crate) zero: *mut u8,
    endpoint: PhantomData<E>,
}

impl<E: Endpoint> Exchange<E> {
    pub(crate) fn new() -> Self {
        let zero = alloc(RESULT_SIZE);
        // SAFETY: a fresh result area.
        unsafe { core::ptr::write_bytes(zero, 0, RESULT_SIZE as usize) };
        Self {
            contents: Waitable::default(),
            data: core::ptr::null(),
            left: 0,
            trailers: Waitable::default(),
            end: Waitable::default(),
            outcome: Waitable::default(),
            body: core::ptr::null_mut(),
            transmit: Waitable::default(),
            ended: alloc(RESULT_SIZE),
            transmitted: alloc(RESULT_SIZE),
            zero,
            endpoint: PhantomData,
        }
    }

    pub(crate) fn write(&mut self, contents: u32, trailers: u32, transmit: u32, data: &[u8]) {
        self.contents = Waitable::new(contents);
        self.data = data.as_ptr();
        self.left = data.len() as u32;
        self.trailers = Waitable::new(trailers);
        self.transmit = Waitable::new(transmit);
        self.write_contents();
        self.trailers
            .write(self.zero, E::TRAILERS_WRITE, E::TRAILERS_DROP);
        if self.transmit.read(self.transmitted, E::TRANSMIT_READ) {
            self.transmit.finish(E::TRANSMIT_DROP);
        }
    }

    pub(crate) fn receive(&mut self, message: u32) {
        let mut ends = [0u32; 2];
        // SAFETY: the caller transfers the incoming message; the result area is fresh.
        let outcome = unsafe {
            let outcome = (E::OUTCOME_NEW)();
            (E::CONSUME_BODY)(message, outcome as u32, ends.as_mut_ptr());
            outcome
        };
        self.outcome = Waitable::new((outcome >> 32) as u32);
        self.body = body_stream(ends[0]);
        self.end = Waitable::new(ends[1]);
    }

    /// Report consumption before reading trailers so the host pipes the body after the outcome.
    pub(crate) fn read(&mut self) {
        self.outcome
            .write(self.zero, E::OUTCOME_WRITE, E::OUTCOME_DROP);
        if self.end.read(self.ended, E::END_READ) {
            self.ended();
        }
    }

    pub(crate) fn writing(&self) -> bool {
        self.contents.handle != 0 || self.trailers.handle != 0 || self.transmit.handle != 0
    }

    pub(crate) fn busy(&self) -> bool {
        self.writing() || self.end.handle != 0 || self.outcome.handle != 0
    }

    fn write_contents(&mut self) {
        while self.contents.handle != 0 {
            if self.left == 0 {
                self.contents.finish(E::CONTENTS_DROP);
                return;
            }
            // SAFETY: the body lives in the task region until the write completes.
            let code = unsafe { (E::CONTENTS_WRITE)(self.contents.handle, self.data, self.left) };
            if self.contents.wait(code) {
                return;
            }
            self.contents_written(code);
        }
    }

    fn contents_written(&mut self, code: u32) {
        let count = code >> 4;
        // SAFETY: within the body.
        self.data = unsafe { self.data.add(count as usize) };
        self.left -= count;
        if code & 0xF != 0 {
            self.left = 0;
        }
    }

    fn ended(&mut self) {
        // SAFETY: the read wrote `result<option<trailers>, error-code>`.
        let (failed, some, trailers) = unsafe {
            (
                *self.ended != 0,
                *self.ended.add(8) != 0,
                *(self.ended.add(12) as *const u32),
            )
        };
        if !failed && some {
            // SAFETY: trailers handed to this exchange.
            unsafe { fields_drop(trailers) };
        }
        self.end.finish(E::END_DROP);
        complete(self.body, !failed);
    }

    pub(crate) fn cancel_writes(&mut self) {
        self.contents.cancel(E::CONTENTS_CANCEL, E::CONTENTS_DROP);
        self.trailers.cancel(E::TRAILERS_CANCEL, E::TRAILERS_DROP);
    }

    pub(crate) fn event(&mut self, event: u32, handle: u32, code: u32) -> bool {
        if handle == 0 {
            return false;
        }
        match event {
            perry_abi::EVENT_STREAM_WRITE if self.contents.handle == handle => {
                self.contents.blocked = false;
                self.contents_written(code);
                self.write_contents();
            }
            EVENT_FUTURE_WRITE if self.trailers.handle == handle => {
                self.trailers.finish(E::TRAILERS_DROP)
            }
            EVENT_FUTURE_WRITE if self.outcome.handle == handle => {
                self.outcome.finish(E::OUTCOME_DROP)
            }
            perry_abi::EVENT_FUTURE_READ if self.end.handle == handle => self.ended(),
            perry_abi::EVENT_FUTURE_READ if self.transmit.handle == handle => {
                self.transmit.finish(E::TRANSMIT_DROP)
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn close(&mut self) {
        if E::OUTGOING_REQUEST {
            self.cancel_writes();
            self.end.cancel(E::END_CANCEL, E::END_DROP);
            self.outcome.cancel(E::OUTCOME_CANCEL, E::OUTCOME_DROP);
        } else {
            self.outcome.cancel(E::OUTCOME_CANCEL, E::OUTCOME_DROP);
            self.end.cancel(E::END_CANCEL, E::END_DROP);
            self.cancel_writes();
        }
        self.transmit.cancel(E::TRANSMIT_CANCEL, E::TRANSMIT_DROP);
    }
}

const EVENT_FUTURE_WRITE: u32 = 5;
