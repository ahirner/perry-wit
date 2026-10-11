//! Tasks and their regions: bump allocation over a chain of chunks, released in bulk when the
//! task ends.
//!
//! Every export call is one task (design decision D3/D5). Async export calls can overlap on one
//! instance, so each task owns its region, its queue of ready frames and its pending host
//! subtasks ([Task]). Compiled code makes a task current while it runs ([perry_task_resume]) and
//! clears it before handing control back to the host ([perry_task_suspend]).
//!
//! At a component export, the host lowers string and list arguments through
//! [perry_cabi_realloc] before the export runs. With no current task, that creates the task
//! early; the export adopts it with [perry_task_enter]. Synchronous exports end the task in
//! their post-return function, after the host has lifted the results, so arguments and results
//! live in the region and are released with everything else.
use crate::instance::instance_state;
#[cfg(target_arch = "wasm32")]
use alloc::alloc::{Layout, alloc, dealloc};
#[cfg(target_arch = "wasm32")]
use core::cell::UnsafeCell;
use core::ptr;

/// Size of the largest standard chunk, including its header. A task's chunks start at
/// [MIN_CHUNK] and double up to this size, so a task that allocates little claims little
/// memory (components can run in a single 64 KiB page).
pub(crate) const CHUNK: usize = 64 * 1024;
const MIN_CHUNK: usize = 4 * 1024;
/// Standard chunk sizes: 4, 8, 16, 32 and 64 KiB, each with a free list.
const CLASSES: usize = 5;
/// Allocations above this size get a dedicated chunk.
const LARGE: usize = CHUNK / 4;
const ALIGN: usize = 8;

#[repr(C)]
struct Chunk {
    previous: *mut Chunk,
    size: usize,
}

const HEADER: usize = core::mem::size_of::<Chunk>().next_multiple_of(ALIGN);

/// A frame waiting to run or waiting on a promise.
#[repr(C)]
pub(crate) struct Node {
    pub(crate) frame: *mut u8,
    pub(crate) next: *mut Node,
}

/// A host subtask whose completion settles a promise.
#[repr(C)]
pub(crate) struct Subtask {
    pub(crate) handle: u32,
    foreign: bool,
    pub(crate) promise: *mut u8,
    value: u64,
    pub(crate) next: *mut Subtask,
    pub(crate) owner: *mut Task,
    pub(crate) signal: *mut crate::abort::Signal,
    pub(crate) watch_previous: *mut Subtask,
    pub(crate) watch_next: *mut Subtask,
}

/// One export call.
#[repr(C)]
pub struct Task {
    chunk: *mut Chunk,
    large: *mut Chunk,
    cursor: usize,
    end: usize,
    /// Size class of `chunk`.
    class: usize,
    ready_head: *mut Node,
    ready_tail: *mut Node,
    /// Queue nodes and subtask entries no longer in use: waits and wakeups in a loop reuse
    /// them instead of growing the region.
    spare_nodes: *mut Node,
    spare_subtasks: *mut Subtask,
    pub(crate) subtasks: *mut Subtask,
    cancelled: *mut Subtask,
    /// Waitable set of the pending subtasks and output streams, created on first use; 0 if
    /// none.
    pub(crate) set: u32,
    /// The promise an async export settles; read again by its callback.
    result: *mut u8,
    /// The thrown exception object on its way to a handler (`crate::exception`); null when
    /// the status alone describes it.
    pub(crate) exception: *mut u8,
    /// Console output (`crate::io`): stdout, stderr.
    pub(crate) outputs: [crate::io::Output; 2],
    /// A command's result once decided (`crate::io::perry_command_outcome`).
    pub(crate) outcome: Option<u32>,
    /// Status of the last runtime service that threw (`crate::exception::raise`), taken by
    /// compiled code with `perry_status`.
    pub(crate) status: u32,
}

impl Task {
    /// Active scheduler edges; recycled nodes and allocation history never retain objects.
    pub(crate) unsafe fn trace_roots(&self, roots: &mut alloc::vec::Vec<*mut u8>) {
        roots.extend([self.result, self.exception]);
        // SAFETY: active queue entries belong to this live task.
        unsafe {
            let mut node = self.ready_head;
            while !node.is_null() {
                roots.push((*node).frame);
                node = (*node).next;
            }
            let mut subtask = self.subtasks;
            while !subtask.is_null() {
                roots.extend([(*subtask).promise, (*subtask).signal.cast()]);
                if crate::promise::has_reference_value((*subtask).promise) {
                    roots.push((*subtask).value as usize as *mut u8);
                }
                subtask = (*subtask).next;
            }
            let mut cancelled = self.cancelled;
            while !cancelled.is_null() {
                roots.push((*cancelled).value as usize as *mut u8);
                cancelled = (*cancelled).next;
            }
            for output in &self.outputs {
                output.trace_roots(roots);
            }
        }
    }
}

pub(crate) struct Runtime {
    current: *mut Task,
    free: [*mut Chunk; CLASSES],
}

impl Runtime {
    pub(crate) const fn new() -> Self {
        Self {
            current: ptr::null_mut(),
            free: [ptr::null_mut(); CLASSES],
        }
    }
}

/// Native allocations have an owner even when an instance is dropped with suspended tasks.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(crate) struct NativeStorage {
    #[expect(
        clippy::vec_box,
        reason = "ABI task pointers must survive vector growth"
    )]
    tasks: Vec<Box<Task>>,
    chunks: Vec<Box<[u64]>>,
}

#[cfg(target_arch = "wasm32")]
struct Global(UnsafeCell<Runtime>);
// SAFETY: one instance runs one thread (the callback ABI has no shared-memory threads).
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for Global {}

instance_state!(RUNTIME, runtime_state, runtime, Runtime, Runtime::new());

#[allow(clippy::mut_from_ref)]
fn runtime() -> &'static mut Runtime {
    // SAFETY: single-threaded; no reference outlives one runtime entry point.
    unsafe { &mut *runtime_state() }
}

/// The running task. Compiled code only allocates while a task is current.
pub(crate) fn current() -> &'static mut Task {
    let task = runtime().current;
    if task.is_null() {
        out_of_task();
    }
    // SAFETY: tasks stay allocated until [perry_task_free].
    unsafe { &mut *task }
}

#[cfg(target_arch = "wasm32")]
fn layout(size: usize) -> Layout {
    // A size beyond the address space cannot be allocated either.
    Layout::from_size_align(size, ALIGN).unwrap_or_else(|_| out_of_memory())
}

fn new_chunk(size: usize, previous: *mut Chunk) -> *mut Chunk {
    // SAFETY: the layout has non-zero size.
    #[cfg(target_arch = "wasm32")]
    let chunk = unsafe { alloc(layout(size)) } as *mut Chunk;
    #[cfg(not(target_arch = "wasm32"))]
    let chunk = crate::instance::native::with(|instance| {
        let mut memory = vec![0u64; size.div_ceil(ALIGN)].into_boxed_slice();
        let pointer = memory.as_mut_ptr().cast::<Chunk>();
        instance.storage.borrow_mut().chunks.push(memory);
        pointer
    });
    if chunk.is_null() {
        out_of_memory();
    }
    // SAFETY: `chunk` points to at least HEADER writable bytes.
    unsafe { chunk.write(Chunk { previous, size }) };
    chunk
}

fn new_task() -> *mut Task {
    let value = Task {
        chunk: ptr::null_mut(),
        large: ptr::null_mut(),
        cursor: 0,
        end: 0,
        class: 0,
        ready_head: ptr::null_mut(),
        ready_tail: ptr::null_mut(),
        spare_nodes: ptr::null_mut(),
        spare_subtasks: ptr::null_mut(),
        subtasks: ptr::null_mut(),
        cancelled: ptr::null_mut(),
        set: 0,
        result: ptr::null_mut(),
        exception: ptr::null_mut(),
        outputs: [crate::io::Output::new(), crate::io::Output::new()],
        outcome: None,
        status: 0,
    };
    #[cfg(target_arch = "wasm32")]
    {
        // SAFETY: the layout has non-zero size.
        let task = unsafe { alloc(Layout::new::<Task>()) } as *mut Task;
        if task.is_null() {
            out_of_memory();
        }
        // SAFETY: freshly allocated.
        unsafe { task.write(value) };
        task
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        crate::instance::native::with(|instance| {
            let mut task = Box::new(value);
            let pointer = ptr::from_mut(&mut *task);
            instance.storage.borrow_mut().tasks.push(task);
            pointer
        })
    }
}

impl Task {
    pub(crate) fn allocate(&mut self, size: usize) -> *mut u8 {
        let size = size.next_multiple_of(ALIGN).max(ALIGN);
        if size > LARGE {
            let chunk = new_chunk(HEADER + size, self.large);
            self.large = chunk;
            return (chunk as usize + HEADER) as *mut u8;
        }
        if size > self.end - self.cursor {
            let mut class = if self.chunk.is_null() {
                0
            } else {
                (self.class + 1).min(CLASSES - 1)
            };
            while (MIN_CHUNK << class) - HEADER < size {
                class += 1;
            }
            let chunk_size = MIN_CHUNK << class;
            let free = &mut runtime().free[class];
            let chunk = if free.is_null() {
                new_chunk(chunk_size, self.chunk)
            } else {
                let chunk = *free;
                // SAFETY: free-list chunks are live allocations of this class owned here.
                unsafe {
                    *free = (*chunk).previous;
                    (*chunk).previous = self.chunk;
                }
                chunk
            };
            self.chunk = chunk;
            self.class = class;
            self.cursor = chunk as usize + HEADER;
            self.end = chunk as usize + chunk_size;
        }
        let object = self.cursor as *mut u8;
        self.cursor += size;
        object
    }

    /// A queue node for `frame`, reused when one is spare.
    pub(crate) fn node(&mut self, frame: *mut u8) -> *mut Node {
        let node = if self.spare_nodes.is_null() {
            self.allocate(core::mem::size_of::<Node>()) as *mut Node
        } else {
            let node = self.spare_nodes;
            // SAFETY: spare nodes are live allocations of this task.
            self.spare_nodes = unsafe { (*node).next };
            node
        };
        // SAFETY: `node` is exclusively ours.
        unsafe {
            node.write(Node {
                frame,
                next: ptr::null_mut(),
            })
        };
        node
    }

    /// Queue `node` (a waiter whose promise settled) as it is.
    pub(crate) fn push_ready_node(&mut self, node: *mut Node) {
        // SAFETY: `node` is a live node of this task; the tail, if any, too.
        unsafe {
            (*node).next = ptr::null_mut();
            if self.ready_tail.is_null() {
                self.ready_head = node;
            } else {
                (*self.ready_tail).next = node;
            }
        }
        self.ready_tail = node;
    }
}

fn out_of_memory() -> ! {
    #[cfg(target_arch = "wasm32")]
    core::arch::wasm32::unreachable();
    #[cfg(not(target_arch = "wasm32"))]
    panic!("perry runtime out of memory");
}

fn out_of_task() -> ! {
    #[cfg(target_arch = "wasm32")]
    core::arch::wasm32::unreachable();
    #[cfg(not(target_arch = "wasm32"))]
    panic!("perry runtime used outside a task");
}

/// Enter an export's task: adopt the one argument lowering created, or create one. Returns the
/// task, which async exports keep in `context.set 0` for their callback.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_enter() -> *mut Task {
    let runtime = runtime();
    if runtime.current.is_null() {
        runtime.current = new_task();
    }
    runtime.current
}

/// The current task, as an identity for libraries that keep per-task state.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_current() -> *mut Task {
    runtime().current
}

/// Make `task` current again (callback entry).
///
/// # Safety
/// `task` must come from [perry_task_enter] in the active instance and not be freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_task_resume(task: *mut Task) {
    runtime().current = task;
}

/// Leave the current task pending, before returning `WAIT` to the host. Returns its waitable
/// set (0 if it has no pending subtask, which would wait forever).
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_suspend() -> u32 {
    let set = current().set;
    runtime().current = ptr::null_mut();
    set
}

/// End the current task: release its region and the task itself.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_free() {
    let runtime = runtime();
    let task = runtime.current;
    if task.is_null() {
        return;
    }
    // SAFETY: every chunk on the task's chains belongs to it; the task was allocated by
    // [new_task].
    unsafe {
        let mut chunk = (*task).chunk;
        while !chunk.is_null() {
            let previous = (*chunk).previous;
            let class = ((*chunk).size / MIN_CHUNK).trailing_zeros() as usize;
            (*chunk).previous = runtime.free[class];
            runtime.free[class] = chunk;
            chunk = previous;
        }
        let mut large = (*task).large;
        while !large.is_null() {
            let previous = (*large).previous;
            #[cfg(target_arch = "wasm32")]
            dealloc(large as *mut u8, layout((*large).size));
            #[cfg(not(target_arch = "wasm32"))]
            crate::instance::native::with(|instance| {
                instance
                    .storage
                    .borrow_mut()
                    .chunks
                    .retain(|memory| memory.as_ptr().cast::<Chunk>() != large);
            });
            large = previous;
        }
        #[cfg(target_arch = "wasm32")]
        dealloc(task as *mut u8, Layout::new::<Task>());
        #[cfg(not(target_arch = "wasm32"))]
        crate::instance::native::with(|instance| {
            instance
                .storage
                .borrow_mut()
                .tasks
                .retain(|owned| !ptr::eq(&**owned, task));
        });
    }
    runtime.current = ptr::null_mut();
}

/// The current task's waitable set, or 0.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_task_set() -> u32 {
    current().set
}

/// Record the waitable set created for the current task.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_task_set_set(set: u32) {
    current().set = set;
}

/// The promise the current async export settles.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_task_result() -> *mut u8 {
    current().result
}

/// Record the promise the current async export settles.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_task_set_result(promise: *mut u8) {
    current().result = promise;
}

/// The next frame ready to run in the current task, or null.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_ready_pop() -> *mut u8 {
    let task = current();
    let node = task.ready_head;
    if node.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: queue nodes are live allocations of this task.
    unsafe {
        task.ready_head = (*node).next;
        if task.ready_head.is_null() {
            task.ready_tail = ptr::null_mut();
        }
        let frame = (*node).frame;
        (*node).frame = ptr::null_mut();
        (*node).next = task.spare_nodes;
        task.spare_nodes = node;
        frame
    }
}

/// Settle `promise` with `value` when host subtask `handle` returns.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_subtask_register(handle: u32, promise: *mut u8, value: u64) {
    register_subtask(handle, promise, value, false);
}

/// Register a typed canonical transfer whose cancelled result needs compiler-generated disposal.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_foreign_register(handle: u32, promise: *mut u8, value: u64) {
    register_subtask(handle, promise, value, true);
}

fn register_subtask(handle: u32, promise: *mut u8, value: u64, foreign: bool) {
    let task = current();
    let entry = if task.spare_subtasks.is_null() {
        task.allocate(core::mem::size_of::<Subtask>()) as *mut Subtask
    } else {
        let entry = task.spare_subtasks;
        // SAFETY: spare entries are live allocations of this task.
        task.spare_subtasks = unsafe { (*entry).next };
        entry
    };
    // SAFETY: freshly allocated.
    unsafe {
        entry.write(Subtask {
            handle,
            foreign,
            promise,
            value,
            next: task.subtasks,
            owner: task,
            signal: ptr::null_mut(),
            watch_previous: ptr::null_mut(),
            watch_next: ptr::null_mut(),
        })
    };
    task.subtasks = entry;
}

/// Host subtask `handle` returned: settle its promise. Returns 0 if `handle` is unknown.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_subtask_done(handle: u32) -> u32 {
    if let Some((promise, value)) = take_subtask(handle) {
        crate::promise::perry_promise_resolve(promise, value);
        1
    } else {
        0
    }
}

/// Remove a completed timer or transfer, severing its roots before recycling the entry.
pub(crate) fn take_subtask(handle: u32) -> Option<(*mut u8, u64)> {
    let task = current();
    let mut link: *mut *mut Subtask = &mut task.subtasks;
    // SAFETY: the list holds live entries of this task.
    unsafe {
        while !(*link).is_null() {
            let entry = *link;
            if (*entry).handle == handle {
                *link = (*entry).next;
                crate::abort::detach(entry);
                let value = ((*entry).promise, (*entry).value);
                (*entry).promise = ptr::null_mut();
                (*entry).value = 0;
                (*entry).next = task.spare_subtasks;
                task.spare_subtasks = entry;
                return Some(value);
            }
            link = &mut (*entry).next;
        }
    }
    None
}

/// Cancel unobserved host calls before their task storage is reclaimed.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_task_cancel() -> u32 {
    let task = perry_task_current();
    let mut count = 0;
    // SAFETY: cancellation acknowledges each host call; the task remains live throughout.
    unsafe {
        while !(*task).subtasks.is_null() {
            let entry = (*task).subtasks;
            let handle = (*entry).handle;
            crate::host::waitable_join(handle, 0);
            let status = crate::host::subtask_cancel(handle);
            perry_task_resume(task);
            crate::host::subtask_drop(handle);
            (*task).subtasks = (*entry).next;
            crate::abort::detach(entry);
            (*entry).promise = ptr::null_mut();
            if (*entry).foreign {
                let transfer = (*entry).value as usize as *mut u8;
                transfer
                    .add(crate::foreign::RETURNED as usize)
                    .cast::<u32>()
                    .write(u32::from(status == crate::host::SUBTASK_RETURNED));
                (*entry).next = (*task).cancelled;
                (*task).cancelled = entry;
            } else {
                (*entry).value = 0;
                (*entry).next = (*task).spare_subtasks;
                (*task).spare_subtasks = entry;
            }
            count += 1;
        }
    }
    count
}

/// Take a cancelled transfer after acknowledgement. Disposal does not introduce safe points.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_foreign_cancelled() -> *mut u8 {
    let task = current();
    let entry = task.cancelled;
    if entry.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: cancellation queues live scheduler nodes until the compiled cleanup drains them.
    unsafe {
        task.cancelled = (*entry).next;
        let transfer = (*entry).value as usize as *mut u8;
        (*entry).value = 0;
        (*entry).next = task.spare_subtasks;
        task.spare_subtasks = entry;
        transfer
    }
}

/// The canonical ABI `realloc`, exported by compiled components as `cabi_realloc`.
///
/// The host only allocates (`old == 0`) to lower arguments and results; blocks live in the
/// current task's region, created here when argument lowering precedes the export call, and
/// are never freed individually. Alignment is at most 8 in the canonical ABI.
///
/// # Safety
/// `old` must be null or a block of `old_size` bytes from this function.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_cabi_realloc(
    old: *mut u8,
    old_size: u32,
    _align: u32,
    new_size: u32,
) -> *mut u8 {
    #[cfg(feature = "collectible")]
    {
        unsafe { crate::buffers::reallocate(old, old_size, new_size) }
    }
    #[cfg(not(feature = "collectible"))]
    {
        perry_task_enter();
        if new_size == 0 {
            return ALIGN as *mut u8;
        }
        let block = current().allocate(new_size as usize);
        if !old.is_null() {
            // SAFETY: both blocks are live and distinct; the copy stays within both.
            unsafe { ptr::copy_nonoverlapping(old, block, old_size.min(new_size) as usize) };
        }
        block
    }
}

/// Release the host's canonical pin after its value is lifted or discarded.
///
/// # Safety
/// `data` is a pointer produced by [perry_cabi_realloc].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_cabi_release(data: *mut u8) {
    #[cfg(feature = "collectible")]
    unsafe {
        crate::buffers::release(data)
    };
    #[cfg(not(feature = "collectible"))]
    let _ = data;
}

/// Allocate `size` bytes, 8-aligned, in the current task's region. Never returns null.
#[unsafe(no_mangle)]
pub extern "C" fn perry_alloc(size: u32) -> *mut u8 {
    // SAFETY: an opaque allocation has no traced fields.
    unsafe { perry_alloc_shaped(size, ptr::null()) }
}

/// Allocate an object with a static map of its reference fields, without collecting.
///
/// # Safety
/// `shape` meets `Collector::allocate`'s shape contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_alloc_shaped(size: u32, shape: *const u32) -> *mut u8 {
    #[cfg(feature = "collectible")]
    {
        unsafe { crate::roots::perry_gc_alloc(size, shape) }
    }
    #[cfg(not(feature = "collectible"))]
    {
        let _ = shape;
        current().allocate(size as usize)
    }
}

/// Grow private helper storage; collectible payloads first attempt non-moving growth.
///
/// # Safety
/// `old` is null or exclusively owned storage of `old_size` bytes. `initialized <= old_size`
/// bytes are live. No immutable aliases or outstanding host borrows may refer to this buffer.
/// `shape` describes both capacities; allocation remains atomic with respect to collection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_buffer_resize(
    old: *mut u8,
    old_size: u32,
    new_size: u32,
    initialized: u32,
    shape: *const u32,
) -> *mut u8 {
    #[cfg(feature = "collectible")]
    {
        unsafe { crate::roots::resize_buffer(old, old_size, new_size, initialized, shape) }
    }
    #[cfg(not(feature = "collectible"))]
    {
        assert!(
            initialized <= old_size && old_size <= new_size,
            "buffer resize bounds"
        );
        let new = unsafe { perry_alloc_shaped(new_size, shape) };
        if initialized != 0 {
            unsafe { ptr::copy_nonoverlapping(old, new, initialized as usize) };
        }
        new
    }
}

/// Pin an active runtime operation; bounded-region components need no pin table.
#[unsafe(no_mangle)]
pub extern "C" fn perry_operation_pin(owner: *mut u8) -> u32 {
    #[cfg(feature = "collectible")]
    {
        crate::roots::perry_gc_pin(owner)
    }
    #[cfg(not(feature = "collectible"))]
    {
        let _ = owner;
        0
    }
}

/// Release a completed operation only after its host transfers are acknowledged.
#[unsafe(no_mangle)]
pub extern "C" fn perry_operation_unpin(token: u32) {
    #[cfg(feature = "collectible")]
    if token != 0 {
        crate::roots::perry_gc_unpin(token);
    }
    #[cfg(not(feature = "collectible"))]
    let _ = token;
}

/// Chunks in use and dedicated chunks of the current task (0, 0 without one), and chunks on
/// the free list.
#[cfg(test)]
pub(crate) fn chunk_counts() -> (usize, usize, usize) {
    let count = |mut chunk: *mut Chunk| {
        let mut n = 0;
        while !chunk.is_null() {
            n += 1;
            // SAFETY: chunks in every list are live.
            chunk = unsafe { (*chunk).previous };
        }
        n
    };
    let runtime = runtime();
    let (chunks, large) = if runtime.current.is_null() {
        (0, 0)
    } else {
        // SAFETY: the current task is live.
        unsafe {
            (
                count((*runtime.current).chunk),
                count((*runtime.current).large),
            )
        }
    };
    (
        chunks,
        large,
        runtime.free.iter().map(|&free| count(free)).sum(),
    )
}
