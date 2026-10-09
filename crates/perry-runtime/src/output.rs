//! Task-owned P3 output queues. Each channel drains in call order, including the host's
//! completion future, before the command may release its region.
use crate::{region::current, string};
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
}

struct Queue {
    head: *mut Write,
    tail: *mut Write,
}

macro_rules! channel {
    ($module:ident, $interface:literal, $index:expr, $write:ident, $event:ident) => {
        mod $module {
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
            }

            /// # Safety
            /// `text` is a task-owned string; its bytes stay live until output completes.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn $write(text: *mut u8) {
                // SAFETY: queue and nodes are exclusively owned by the current task.
                unsafe {
                    let bytes = string::bytes(text);
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

            unsafe fn drive(queue: *mut Queue, mut event: Option<u32>) {
                // SAFETY: one callback at a time accesses task-owned nodes and canonical handles.
                unsafe {
                    while !(*queue).head.is_null() {
                        let node = &mut *(*queue).head;
                        match node.phase {
                            Phase::Queued => {
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
                                    current().output_failed |= result & 15 != 0;
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
                                current().output_failed |= result != 0 || node.completion[0] != 0;
                                future_drop(future);
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
    perry_stdout_event
);
channel!(
    stderr,
    "wasi:cli/stderr@0.3.0",
    1,
    perry_stderr,
    perry_stderr_event
);
