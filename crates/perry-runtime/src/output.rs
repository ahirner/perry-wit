//! Task-owned P3 output queues. Each channel drains in call order, including the host's
//! completion future, before the command may release its region.
use crate::{region::current, string, writable::Stream};
use core::ptr;

#[link(wasm_import_module = "$root")]
unsafe extern "C" {
    #[link_name = "[waitable-set-new]"]
    fn set_new() -> u32;
    #[link_name = "[waitable-join]"]
    fn join(handle: u32, set: u32);
}

fn wait(handle: u32) {
    let task = current();
    // SAFETY: canonical handles belong to this task; zero denotes no set.
    unsafe {
        if task.set == 0 {
            task.set = set_new();
        }
        join(handle, task.set);
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Queued,
    Writing {
        writer: u32,
        future: u32,
        offset: u32,
    },
    Completion {
        future: u32,
    },
}

struct Write {
    next: *mut Write,
    data: *const u8,
    len: u32,
    completion: [u8; 2],
    phase: Phase,
    destination: Destination,
}

#[derive(Clone, Copy)]
pub(crate) enum Destination {
    Console,
    Writer {
        stream: *mut Stream,
        promise: *mut u8,
    },
}

impl Destination {
    unsafe fn failed(self) -> bool {
        match self {
            Self::Console => false,
            // SAFETY: the queue retains the task-owned stream.
            Self::Writer { stream, .. } => unsafe { (*stream).failed() },
        }
    }

    unsafe fn fail(self) {
        match self {
            Self::Console => current().output_failed = true,
            // SAFETY: the queue retains the task-owned stream.
            Self::Writer { stream, .. } => unsafe { (*stream).fail() },
        }
    }

    unsafe fn complete(self) {
        if let Self::Writer { stream, promise } = self {
            // SAFETY: the queue retains the task-owned stream and promise.
            unsafe { (*stream).complete(promise) };
        }
    }
}

struct Queue {
    head: *mut Write,
    tail: *mut Write,
}

macro_rules! channel {
    ($module:ident, $interface:literal, $index:expr, $write:ident, $event:ident, $cancel:ident) => {
        pub(crate) mod $module {
            use super::*;
            #[link(wasm_import_module = $interface)]
            unsafe extern "C" {
                #[link_name = "[stream-new-0]write-via-stream"]
                fn stream_new() -> u64;
                #[link_name = "write-via-stream"]
                fn start(reader: u32) -> u32;
                #[link_name = "[async-lower][stream-write-0]write-via-stream"]
                fn stream_write(writer: u32, data: *const u8, len: u32) -> u32;
                #[link_name = "[stream-drop-writable-0]write-via-stream"]
                fn stream_drop(writer: u32);
                #[link_name = "[async-lower][future-read-1]write-via-stream"]
                fn future_read(future: u32, result: *mut u8) -> u32;
                #[link_name = "[future-drop-readable-1]write-via-stream"]
                fn future_drop(future: u32);
                #[link_name = "[stream-cancel-write-0]write-via-stream"]
                fn stream_cancel(writer: u32) -> u32;
                #[link_name = "[future-cancel-read-1]write-via-stream"]
                fn future_cancel(future: u32) -> u32;
            }

            /// # Safety
            /// `text` is a task-owned string; its bytes stay live until output completes.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $write(text: *mut u8) {
                // SAFETY: text belongs to the current task.
                unsafe { enqueue(string::bytes(text), Destination::Console) };
            }

            pub(crate) unsafe fn enqueue(bytes: &[u8], destination: Destination) {
                // SAFETY: queue and nodes are exclusively owned by the current task.
                unsafe {
                    let task = current();
                    let mut queue = task.outputs[$index] as *mut Queue;
                    let idle = queue.is_null();
                    if idle {
                        queue = task.allocate(size_of::<Queue>()) as *mut Queue;
                        queue.write(Queue {
                            head: ptr::null_mut(),
                            tail: ptr::null_mut(),
                        });
                        task.outputs[$index] = queue.cast();
                    }
                    let node = task.allocate(size_of::<Write>()) as *mut Write;
                    node.write(Write {
                        next: ptr::null_mut(),
                        data: bytes.as_ptr(),
                        len: bytes.len() as u32,
                        completion: [0; 2],
                        phase: Phase::Queued,
                        destination,
                    });
                    if (*queue).head.is_null() {
                        (*queue).head = node;
                    } else {
                        (*(*queue).tail).next = node;
                    }
                    (*queue).tail = node;
                    if idle {
                        drive(queue, None);
                    }
                }
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn $event(event: u32, handle: u32, result: u32) {
                let queue = current().outputs[$index] as *mut Queue;
                if queue.is_null() {
                    return;
                }
                // SAFETY: the queue and its pending transfer belong to this task.
                unsafe {
                    let matched = match (*(*queue).head).phase {
                        Phase::Writing { writer, .. } => event == 3 && handle == writer,
                        Phase::Completion { future } => event == 4 && handle == future,
                        Phase::Queued => false,
                    };
                    if matched {
                        drive(queue, Some(result));
                    }
                }
            }

            #[unsafe(no_mangle)]
            pub extern "C" fn $cancel() {
                let task = current() as *mut crate::region::Task;
                // SAFETY: only the head owns native handles; queued writes have not started.
                unsafe {
                    let queue = core::mem::replace(&mut (*task).outputs[$index], ptr::null_mut())
                        as *mut Queue;
                    if queue.is_null() {
                        return;
                    }
                    match (*(*queue).head).phase {
                        Phase::Writing { writer, future, .. } => {
                            join(writer, 0);
                            stream_cancel(writer);
                            crate::region::perry_task_resume(task);
                            stream_drop(writer);
                            future_drop(future);
                        }
                        Phase::Completion { future } => {
                            join(future, 0);
                            future_cancel(future);
                            crate::region::perry_task_resume(task);
                            future_drop(future);
                        }
                        Phase::Queued => unreachable!("an idle queue has no native operation"),
                    }
                }
            }

            unsafe fn drive(queue: *mut Queue, mut event: Option<u32>) {
                // SAFETY: one callback at a time accesses task-owned nodes and canonical handles.
                unsafe {
                    while !(*queue).head.is_null() {
                        let node = &mut *(*queue).head;
                        match node.phase {
                            Phase::Queued => {
                                if node.destination.failed() {
                                    node.destination.complete();
                                    (*queue).head = node.next;
                                    continue;
                                }
                                let pair = stream_new();
                                let future = start(pair as u32);
                                node.phase = Phase::Writing {
                                    writer: (pair >> 32) as u32,
                                    future,
                                    offset: 0,
                                };
                            }
                            Phase::Writing {
                                writer,
                                future,
                                offset,
                            } => {
                                let result = event.take().unwrap_or_else(|| {
                                    stream_write(
                                        writer,
                                        node.data.add(offset as usize),
                                        node.len - offset,
                                    )
                                });
                                if result == u32::MAX {
                                    wait(writer);
                                    return;
                                }
                                let count = result >> 4;
                                assert!(
                                    count <= node.len - offset,
                                    "host wrote beyond requested bytes"
                                );
                                let offset = offset + count;
                                if result & 15 != 0 || offset == node.len {
                                    if result & 15 != 0 {
                                        node.destination.fail();
                                    }
                                    stream_drop(writer);
                                    node.phase = Phase::Completion { future };
                                } else {
                                    assert!(count != 0, "output made no progress");
                                    node.phase = Phase::Writing {
                                        writer,
                                        future,
                                        offset,
                                    };
                                }
                            }
                            Phase::Completion { future } => {
                                let result = event.take().unwrap_or_else(|| {
                                    future_read(future, node.completion.as_mut_ptr())
                                });
                                if result == u32::MAX {
                                    wait(future);
                                    return;
                                }
                                if result != 0 || node.completion[0] != 0 {
                                    node.destination.fail();
                                }
                                future_drop(future);
                                node.destination.complete();
                                (*queue).head = node.next;
                            }
                        }
                    }
                    current().outputs[$index] = ptr::null_mut();
                }
            }
        }
    };
}
channel!(
    stdout,
    "wasi:cli/stdout@0.3.0",
    0,
    perry_stdout,
    perry_stdout_event,
    perry_stdout_cancel
);
channel!(
    stderr,
    "wasi:cli/stderr@0.3.0",
    1,
    perry_stderr,
    perry_stderr_event,
    perry_stderr_cancel
);
