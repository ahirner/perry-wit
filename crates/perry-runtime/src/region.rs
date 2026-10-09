//! Task regions: bump allocation over a chain of chunks, released in bulk at task end.
//!
//! Each export call is one task (the callback ABI makes tasks sequential per instance until
//! P5.4 adds the scheduler), so regions nest as a stack: [perry_task_begin] records the
//! current position and [perry_task_end] returns every chunk taken since.
//!
//! At a component export, the host lowers string and list arguments through
//! [perry_cabi_realloc] before the export runs. With no task active, that opens the task early;
//! the export adopts it with [perry_task_enter], and its post-return function ends it, after
//! the host has lifted the results. Arguments and results therefore live in the task region
//! and are released in bulk with everything else. Standard chunks go
//! to a free list for the next task; allocations above a quarter chunk get a dedicated chunk
//! on a separate chain that goes straight back to the allocator.
use alloc::alloc::{Layout, alloc, dealloc};
use core::{cell::UnsafeCell, ptr};

/// Size of a standard chunk, including its header.
pub const CHUNK: usize = 64 * 1024;
/// Allocations above this size get a dedicated chunk.
const LARGE: usize = CHUNK / 4;
const ALIGN: usize = 8;
const MAX_DEPTH: usize = 32;

#[repr(C)]
struct Chunk {
    previous: *mut Chunk,
    size: usize,
}

const HEADER: usize = core::mem::size_of::<Chunk>().next_multiple_of(ALIGN);

#[derive(Clone, Copy)]
struct Mark {
    chunk: *mut Chunk,
    large: *mut Chunk,
    cursor: usize,
    end: usize,
}

const NO_MARK: Mark = Mark {
    chunk: ptr::null_mut(),
    large: ptr::null_mut(),
    cursor: 0,
    end: 0,
};

struct Regions {
    /// Current position: the chunk bumped into (linked to its predecessors) and the chain of
    /// dedicated chunks.
    top: Mark,
    marks: [Mark; MAX_DEPTH],
    depth: usize,
    /// A task opened by argument lowering that the export has not adopted yet.
    pending: bool,
    free: *mut Chunk,
}

struct Global(UnsafeCell<Regions>);
// SAFETY: one instance runs one thread (the callback ABI has no shared-memory threads).
unsafe impl Sync for Global {}

static REGIONS: Global = Global(UnsafeCell::new(Regions {
    top: NO_MARK,
    marks: [NO_MARK; MAX_DEPTH],
    depth: 0,
    pending: false,
    free: ptr::null_mut(),
}));

#[allow(clippy::mut_from_ref)]
fn regions() -> &'static mut Regions {
    // SAFETY: single-threaded; no reference outlives one runtime entry point.
    unsafe { &mut *REGIONS.0.get() }
}

fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, ALIGN).expect("chunk layout")
}

fn new_chunk(size: usize, previous: *mut Chunk) -> *mut Chunk {
    // SAFETY: the layout has non-zero size.
    let chunk = unsafe { alloc(layout(size)) } as *mut Chunk;
    if chunk.is_null() {
        out_of_memory();
    }
    // SAFETY: `chunk` points to at least HEADER writable bytes.
    unsafe { chunk.write(Chunk { previous, size }) };
    chunk
}

impl Regions {
    fn allocate(&mut self, size: usize) -> *mut u8 {
        let size = size.next_multiple_of(ALIGN).max(ALIGN);
        if size > LARGE {
            let chunk = new_chunk(HEADER + size, self.top.large);
            self.top.large = chunk;
            return (chunk as usize + HEADER) as *mut u8;
        }
        if size > self.top.end - self.top.cursor {
            let chunk = if self.free.is_null() {
                new_chunk(CHUNK, self.top.chunk)
            } else {
                let chunk = self.free;
                // SAFETY: free-list chunks are live CHUNK-sized allocations owned here.
                unsafe {
                    self.free = (*chunk).previous;
                    (*chunk).previous = self.top.chunk;
                }
                chunk
            };
            self.top.chunk = chunk;
            self.top.cursor = chunk as usize + HEADER;
            self.top.end = chunk as usize + CHUNK;
        }
        let object = self.top.cursor as *mut u8;
        self.top.cursor += size;
        object
    }

    fn begin(&mut self) {
        assert!(self.depth < MAX_DEPTH, "task regions nested too deeply");
        self.marks[self.depth] = self.top;
        self.depth += 1;
    }

    fn end(&mut self) {
        assert!(self.depth > 0, "perry_task_end without perry_task_begin");
        self.depth -= 1;
        let mark = self.marks[self.depth];
        let mut chunk = self.top.chunk;
        while chunk != mark.chunk {
            // SAFETY: every chunk above the mark was taken during this task.
            unsafe {
                let previous = (*chunk).previous;
                (*chunk).previous = self.free;
                self.free = chunk;
                chunk = previous;
            }
        }
        let mut large = self.top.large;
        while large != mark.large {
            // SAFETY: as above, for dedicated chunks.
            unsafe {
                let previous = (*large).previous;
                dealloc(large as *mut u8, layout((*large).size));
                large = previous;
            }
        }
        self.top = mark;
    }
}

fn out_of_memory() -> ! {
    #[cfg(target_arch = "wasm32")]
    core::arch::wasm32::unreachable();
    #[cfg(not(target_arch = "wasm32"))]
    panic!("perry runtime out of memory");
}

/// Start a task: allocations until the matching [perry_task_end] belong to it.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_begin() {
    regions().begin();
}

/// End the innermost task and release its region.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_end() {
    regions().end();
}

/// Enter an export's task: adopt the one argument lowering opened, or begin a new one.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_enter() {
    let regions = regions();
    if regions.pending {
        regions.pending = false;
    } else {
        regions.begin();
    }
}

/// The canonical ABI `realloc`, exported by compiled components as `cabi_realloc`.
///
/// The host only allocates (`old == 0`) to lower arguments and results; blocks live in the
/// current task's region and are never freed individually. Alignment is at most 8 in the
/// canonical ABI, which the region always provides.
///
/// # Safety
/// `old` must be null or a block of `old_size` bytes from this function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_cabi_realloc(
    old: *mut u8,
    old_size: u32,
    _align: u32,
    new_size: u32,
) -> *mut u8 {
    let regions = regions();
    if regions.depth == 0 {
        regions.begin();
        regions.pending = true;
    }
    if new_size == 0 {
        return ALIGN as *mut u8;
    }
    let block = regions.allocate(new_size as usize);
    if !old.is_null() {
        // SAFETY: both blocks are live and distinct; the copy stays within both.
        unsafe { ptr::copy_nonoverlapping(old, block, old_size.min(new_size) as usize) };
    }
    block
}

/// Allocate `size` bytes, 8-aligned, in the current task's region. Never returns null.
#[unsafe(no_mangle)]
pub extern "C" fn perry_alloc(size: u32) -> *mut u8 {
    regions().allocate(size as usize)
}

/// Chunks in use, dedicated chunks in use, and chunks on the free list.
pub fn chunk_counts() -> (usize, usize, usize) {
    let regions = regions();
    let count = |mut chunk: *mut Chunk| {
        let mut n = 0;
        while !chunk.is_null() {
            n += 1;
            // SAFETY: chunks in every list are live.
            chunk = unsafe { (*chunk).previous };
        }
        n
    };
    (
        count(regions.top.chunk),
        count(regions.top.large),
        count(regions.free),
    )
}
