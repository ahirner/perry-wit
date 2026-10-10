//! Non-moving collectible storage. Allocation never collects: compiled safe points provide
//! roots before calling `collect`. Shapes describe exact pointer offsets and an optional
//! repetition stride; byte payloads have no shape. Small objects occupy size-class pages,
//! with allocation/mark bits beside the payload and free slots swept on demand.
use alloc::{
    alloc::{alloc_zeroed, dealloc, handle_alloc_error},
    boxed::Box,
    vec::Vec,
};
use core::{
    alloc::Layout,
    ptr::{self, NonNull},
};

const PAGE: usize = 1024;
// Leave room for talc's boundary metadata before the next aligned page. A full aligned
// payload would force the allocator to skip almost another page for every small page.
const PAYLOAD: usize = PAGE - 64;
const MAX_SMALL: usize = PAGE / 2;
const MIN_CELL: usize = 2 * core::mem::size_of::<usize>();
const CLASSES: usize = MAX_SMALL.ilog2() as usize - MIN_CELL.ilog2() as usize + 1;
const NONE: u16 = u16::MAX;
const MIN_THRESHOLD: usize = 256 * 1024;

#[repr(C)]
struct Header {
    shape: *const u32,
    size: usize,
}
const HEADER: usize = core::mem::size_of::<Header>();

struct Page {
    memory: NonNull<u8>,
    layout: Layout,
    cell: usize,
    slots: usize,
    allocated: [u64; 1],
    marked: [u64; 1],
    free: u16,
    sweep_pending: bool,
    poison: bool,
    index: usize,
    listed: bool,
}

impl Page {
    fn new(cell: usize) -> Self {
        let bytes = if cell <= MAX_SMALL {
            PAYLOAD
        } else {
            cell.next_multiple_of(PAGE)
        };
        let layout = Layout::from_size_align(bytes, PAGE).expect("page layout");
        // SAFETY: nonzero layout. The allocator is talc in the guest.
        let memory = NonNull::new(unsafe { alloc_zeroed(layout) })
            .unwrap_or_else(|| handle_alloc_error(layout));
        let slots = if cell > MAX_SMALL { 1 } else { PAYLOAD / cell };
        let mut page = Self {
            memory,
            layout,
            cell,
            slots,
            allocated: [0; 1],
            marked: [0; 1],
            free: NONE,
            sweep_pending: false,
            poison: cfg!(test),
            index: 0,
            listed: false,
        };
        for index in (0..slots).rev() {
            page.release(index);
        }
        page
    }

    fn slot(&self, index: usize) -> *mut u8 {
        // SAFETY: callers use an index below `slots`.
        unsafe { self.memory.as_ptr().add(index * self.cell) }
    }

    fn release(&mut self, index: usize) {
        let slot = self.slot(index);
        // SAFETY: the entire slot belongs to this page and is no longer reachable.
        unsafe {
            if self.poison {
                ptr::write_bytes(slot, 0xdd, self.cell);
            }
            (slot as *mut u16).write(self.free);
        }
        self.free = index as u16;
        self.allocated[index / 64] &= !(1 << (index % 64));
    }

    fn sweep(&mut self) {
        if !self.sweep_pending {
            return;
        }
        for index in 0..self.slots {
            let bit = 1 << (index % 64);
            if self.allocated[index / 64] & bit != 0 && self.marked[index / 64] & bit == 0 {
                self.release(index);
            }
        }
        self.sweep_pending = false;
    }

    fn allocate(&mut self, size: usize, shape: *const u32) -> *mut u8 {
        let index = usize::from(self.free);
        let slot = self.slot(index);
        // SAFETY: `free` names a free slot, large enough for header and payload.
        unsafe {
            self.free = (slot as *const u16).read();
            (slot as *mut Header).write(Header { shape, size });
            ptr::write_bytes(slot.add(HEADER), 0, self.cell - HEADER);
        }
        self.allocated[index / 64] |= 1 << (index % 64);
        self.marked[index / 64] |= 1 << (index % 64);
        // SAFETY: every cell reserves HEADER bytes.
        unsafe { slot.add(HEADER) }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        // SAFETY: this page owns the allocation with its current layout.
        unsafe { dealloc(self.memory.as_ptr(), self.layout) };
    }
}

/// Two-level direct lookup for the guest's 32-bit address space. The host test version uses
/// the host address space without reserving a directory proportional to native addresses.
#[cfg(target_arch = "wasm32")]
#[derive(Default)]
struct Directory {
    tables: Vec<Option<Box<[*mut Page; 1024]>>>,
}
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct Directory {
    pages: alloc::collections::BTreeMap<usize, *mut Page>,
}

impl Directory {
    fn get(&self, address: usize) -> *mut Page {
        let number = address / PAGE;
        #[cfg(target_arch = "wasm32")]
        {
            self.tables
                .get(number >> 10)
                .and_then(Option::as_ref)
                .map_or(ptr::null_mut(), |table| table[number & 1023])
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.pages.get(&number).copied().unwrap_or(ptr::null_mut())
        }
    }

    fn set(&mut self, address: usize, page: *mut Page) {
        let number = address / PAGE;
        #[cfg(target_arch = "wasm32")]
        {
            let high = number >> 10;
            if self.tables.len() <= high {
                self.tables.resize_with(high + 1, || None);
            }
            let table = self.tables[high].get_or_insert_with(|| {
                // SAFETY: all-zero bytes represent null pointers; initialize directly on the heap.
                unsafe { Box::<[*mut Page; 1024]>::new_zeroed().assume_init() }
            });
            table[number & 1023] = page;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if page.is_null() {
                self.pages.remove(&number);
            } else {
                self.pages.insert(number, page);
            }
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Counters {
    pub allocations: u64,
    pub collections: u64,
    pub live_bytes: usize,
    pub reserved_bytes: usize,
}

/// Shapes are static arrays `[stride, number_of_offsets, offsets…]`. A zero stride traces
/// one object; a nonzero stride repeats the offsets across its allocation (array storage).
/// Offsets identify native pointer fields, never integer or floating-point slots.
pub(crate) struct Collector {
    pages: Vec<Option<Box<Page>>>,
    directory: Directory,
    available: [Vec<usize>; CLASSES],
    vacant: Vec<usize>,
    work: Vec<*mut u8>,
    volume: usize,
    threshold: usize,
    pressure_threshold: usize,
    poison: bool,
    counters: Counters,
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector {
    pub(crate) const fn new() -> Self {
        Self {
            pages: Vec::new(),
            directory: Directory::new(),
            available: [const { Vec::new() }; CLASSES],
            vacant: Vec::new(),
            work: Vec::new(),
            volume: 0,
            threshold: MIN_THRESHOLD,
            pressure_threshold: 4096,
            poison: false,
            counters: Counters {
                allocations: 0,
                collections: 0,
                live_bytes: 0,
                reserved_bytes: 0,
            },
        }
    }

    /// Allocate without collecting. The caller must establish safe points between allocation
    /// bursts; every pointer field remains zero until the caller initializes it.
    ///
    /// # Safety
    /// `shape` is null or a static shape as documented on [Collector]. All described pointer
    /// fields must lie inside each object/repetition and be initialized before collection.
    pub(crate) unsafe fn allocate(&mut self, size: usize, shape: *const u32) -> *mut u8 {
        let size = size.max(1);
        let required = size.checked_add(HEADER).expect("allocation size overflow");
        let cell = if required <= MAX_SMALL {
            required.next_power_of_two().max(MIN_CELL)
        } else {
            required.next_multiple_of(core::mem::align_of::<Header>())
        };
        let class = (cell <= MAX_SMALL).then(|| cell.ilog2() as usize - MIN_CELL.ilog2() as usize);
        let existing = class.and_then(|class| {
            while let Some(&index) = self.available[class].last() {
                let page = self.pages[index].as_mut().expect("available page");
                page.sweep();
                if page.free != NONE {
                    return Some(index);
                }
                page.listed = false;
                self.available[class].pop();
            }
            None
        });
        let index = existing.unwrap_or_else(|| {
            // Keep allocation metadata below the payload so a large tail block can grow.
            let mut page = if cell > MAX_SMALL {
                if self.vacant.is_empty() {
                    self.pages.reserve(1);
                }
                let storage = Box::<Page>::new_uninit();
                Box::write(storage, Page::new(cell))
            } else {
                Box::new(Page::new(cell))
            };
            page.poison |= self.poison;
            let pointer = &mut *page as *mut Page;
            for offset in (0..page.layout.size()).step_by(PAGE) {
                self.directory
                    .set(page.memory.as_ptr() as usize + offset, pointer);
            }
            self.counters.reserved_bytes += page.layout.size();
            let index = if let Some(index) = self.vacant.pop() {
                self.pages[index] = Some(page);
                index
            } else {
                self.pages.push(Some(page));
                self.pages.len() - 1
            };
            if let Some(class) = class {
                self.available[class].push(index);
                self.pages[index].as_mut().expect("new page").listed = true;
            }
            self.pages[index].as_mut().expect("new page").index = index;
            index
        });
        self.volume = self.volume.saturating_add(size);
        self.counters.allocations += 1;
        self.counters.live_bytes += size;
        self.pages[index]
            .as_mut()
            .expect("page")
            .allocate(size, shape)
    }

    /// Whether `pointer` is inside this exact allocation, including interior host pins.
    pub(crate) fn contains(&self, allocation: *mut u8, pointer: *mut u8) -> bool {
        let page = self.directory.get(allocation as usize);
        if page.is_null() {
            return false;
        }
        // SAFETY: the directory refers to stable owned pages; validate the slot before its header.
        let page = unsafe { &*page };
        let offset = (allocation as usize).wrapping_sub(page.memory.as_ptr() as usize + HEADER);
        let index = offset / page.cell;
        if index >= page.slots
            || !offset.is_multiple_of(page.cell)
            || page.allocated[0] & (1 << index) == 0
        {
            return false;
        }
        let size = unsafe { (*allocation.sub(HEADER).cast::<Header>()).size };
        (pointer as usize).wrapping_sub(allocation as usize) < size
    }

    /// Enlarge an exclusive payload without collecting or moving it. Newly exposed bytes are zero.
    ///
    /// # Safety
    /// `object` is the exact start of a live allocation, with no interior aliases or host borrows.
    /// Its shape remains valid for `size` bytes. Failure preserves its pointer, size and contents.
    pub(crate) unsafe fn grow_in_place(&mut self, object: *mut u8, size: usize) -> bool {
        let page = self.directory.get(object as usize);
        if page.is_null() {
            return false;
        }
        let page = unsafe { &mut *page };
        let offset = (object as usize).wrapping_sub(page.memory.as_ptr() as usize + HEADER);
        let index = offset / page.cell;
        if index >= page.slots
            || !offset.is_multiple_of(page.cell)
            || page.allocated[0] & (1 << index) == 0
        {
            return false;
        }
        let header = unsafe { &mut *object.sub(HEADER).cast::<Header>() };
        let old_size = header.size;
        if size <= old_size {
            return size == old_size;
        }
        let Some(required) = size.checked_add(HEADER) else {
            return false;
        };
        if page.cell <= MAX_SMALL {
            if required > page.cell {
                return false;
            }
        } else {
            let Some(cell) = required.checked_next_multiple_of(core::mem::align_of::<Header>())
            else {
                return false;
            };
            if cell > page.layout.size() {
                let Some(bytes) = cell.checked_next_multiple_of(PAGE) else {
                    return false;
                };
                let Ok(layout) = Layout::from_size_align(bytes, PAGE) else {
                    return false;
                };
                #[cfg(all(target_arch = "wasm32", feature = "collectible"))]
                let grown = unsafe {
                    crate::ALLOCATOR.grow_in_place(page.memory.as_ptr(), page.layout, bytes)
                };
                #[cfg(not(all(target_arch = "wasm32", feature = "collectible")))]
                let grown = false;
                if !grown {
                    return false;
                }
                for offset in (page.layout.size()..bytes).step_by(PAGE) {
                    self.directory
                        .set(page.memory.as_ptr() as usize + offset, page);
                }
                self.counters.reserved_bytes += bytes - page.layout.size();
                page.layout = layout;
            }
            page.cell = cell;
        }
        // SAFETY: the old payload survived growth; the new range is owned and unobserved.
        unsafe { ptr::write_bytes(object.add(old_size), 0, size - old_size) };
        header.size = size;
        self.volume = self.volume.saturating_add(size - old_size);
        self.counters.live_bytes += size - old_size;
        true
    }

    pub(crate) fn due(&self) -> bool {
        if self.volume >= self.threshold {
            return true;
        }
        #[cfg(all(target_arch = "wasm32", feature = "collectible"))]
        if self.volume >= self.pressure_threshold && crate::ALLOCATOR.available() < 8192 {
            return true;
        }
        false
    }
    pub(crate) fn counters(&self) -> Counters {
        self.counters
    }

    pub(crate) fn set_poison(&mut self, enabled: bool) {
        self.poison = enabled;
        for page in self.pages.iter_mut().flatten() {
            page.poison = enabled || cfg!(test);
        }
    }

    /// Release a proven nonescaping small frame at activation exit, without a heap search.
    ///
    /// # Safety
    /// `object` is an exact allocation pointer with no remaining aliases or retained children.
    pub(crate) unsafe fn release_frame(&mut self, object: *mut u8) {
        let page = self.directory.get(object as usize);
        assert!(!page.is_null(), "frame has no page");
        let page = unsafe { &mut *page };
        assert!(page.cell <= MAX_SMALL, "frame exceeds small storage");
        let offset = object as usize - page.memory.as_ptr() as usize - HEADER;
        assert!(
            offset.is_multiple_of(page.cell),
            "frame pointer is not an allocation start"
        );
        let index = offset / page.cell;
        assert!(
            index < page.slots && page.allocated[0] & (1 << index) != 0,
            "frame released twice"
        );
        let size = unsafe { object.sub(HEADER).cast::<Header>().read().size };
        self.counters.live_bytes -= size;
        page.release(index);
        if !page.listed {
            let class = page.cell.ilog2() as usize - MIN_CELL.ilog2() as usize;
            self.available[class].push(page.index);
            page.listed = true;
        }
    }

    fn mark(&mut self, object: *mut u8) {
        let page = self.directory.get(object as usize);
        if page.is_null() {
            return;
        }
        // SAFETY: the directory only holds stable boxed pages owned by this collector.
        let page = unsafe { &mut *page };
        let offset = (object as usize).wrapping_sub(page.memory.as_ptr() as usize);
        if offset < HEADER {
            return;
        }
        let index = (offset - HEADER) / page.cell;
        if index >= page.slots {
            return;
        }
        let payload = page.slot(index);
        // SAFETY: only allocated slots may provide an interior owner.
        if page.allocated[index / 64] & (1 << (index % 64)) == 0 {
            return;
        }
        let size = unsafe { (*(payload as *const Header)).size };
        let within = offset - index * page.cell;
        if within < HEADER || within >= HEADER + size {
            return;
        }
        let (word, bit) = (index / 64, 1 << (index % 64));
        if page.allocated[word] & bit == 0 || page.marked[word] & bit != 0 {
            return;
        }
        page.marked[word] |= bit;
        // SAFETY: the exact pointer lies within this allocated payload.
        self.work.push(unsafe { payload.add(HEADER) });
    }

    /// Trace the supplied precise roots, then make dead small slots available for lazy sweep.
    /// Large dead blocks are returned to talc immediately. Roots may include static objects;
    /// unowned addresses and null sentinels are ignored without inspecting their contents.
    ///
    /// # Safety
    /// All live pointer fields satisfy their allocation's shape. No unregistered temporary
    /// pointer may be needed after this explicit safe point.
    #[cfg(test)]
    pub(crate) unsafe fn collect(&mut self, roots: &[*mut u8]) {
        unsafe { self.collect_with(roots, |_| {}) };
    }

    /// The exact allocation was marked in this collection. Only valid between marking and sweep.
    #[cfg(any(feature = "collectible", not(target_arch = "wasm32")))]
    pub(crate) fn is_marked(&self, object: *mut u8) -> bool {
        let page = self.directory.get(object as usize);
        if page.is_null() {
            return false;
        }
        // SAFETY: directory entries refer to stable, allocated pages.
        let page = unsafe { &*page };
        let offset = (object as usize).wrapping_sub(page.memory.as_ptr() as usize + HEADER);
        let index = offset / page.cell;
        index < page.slots
            && offset.is_multiple_of(page.cell)
            && page.marked[index / 64] & (1 << (index % 64)) != 0
    }

    /// Finalize weak external owners after tracing and before any dead storage is reclaimed.
    /// The callback cannot allocate from this collector or invoke guest/host code.
    ///
    /// # Safety
    /// Same shape and root contract as [`Self::collect`].
    pub(crate) unsafe fn collect_with(&mut self, roots: &[*mut u8], finalize: impl FnOnce(&Self)) {
        for page in self.pages.iter_mut().flatten() {
            // Finish a previous sweep before resetting its marks.
            page.sweep();
            page.marked.fill(0);
        }
        self.work.clear();
        for &root in roots {
            self.mark(root);
        }
        let mut live = 0usize;
        let mut traced = 0usize;
        while let Some(object) = self.work.pop() {
            // SAFETY: mark only enqueues allocated payload starts.
            let header = unsafe { &*object.sub(HEADER).cast::<Header>() };
            live += header.size;
            traced += HEADER;
            if header.shape.is_null() {
                continue;
            }
            traced += header.size;
            // SAFETY: shape validity is guaranteed by allocate's contract.
            let (stride, count) =
                unsafe { (*header.shape as usize, *header.shape.add(1) as usize) };
            let mut base = 0;
            loop {
                for field in 0..count {
                    let offset = unsafe { *header.shape.add(2 + field) as usize };
                    assert!(
                        base + offset + core::mem::size_of::<*mut u8>() <= header.size,
                        "pointer outside allocation"
                    );
                    let child =
                        unsafe { object.add(base + offset).cast::<*mut u8>().read_unaligned() };
                    self.mark(child);
                }
                if stride == 0 || base + stride >= header.size {
                    break;
                }
                base += stride;
            }
        }
        finalize(self);
        for list in &mut self.available {
            list.clear();
        }
        for index in 0..self.pages.len() {
            let Some(page) = &mut self.pages[index] else {
                continue;
            };
            if page.cell > MAX_SMALL && page.marked[0] == 0 {
                for offset in (0..page.layout.size()).step_by(PAGE) {
                    self.directory
                        .set(page.memory.as_ptr() as usize + offset, ptr::null_mut());
                }
                self.counters.reserved_bytes -= page.layout.size();
                self.pages[index] = None;
                self.vacant.push(index);
            } else {
                page.sweep_pending = true;
                if page.cell <= MAX_SMALL {
                    let class = page.cell.ilog2() as usize - MIN_CELL.ilog2() as usize;
                    self.available[class].push(index);
                    page.listed = true;
                }
            }
        }
        // Back off unproductive pressure collections with the traced graph's size. Opaque
        // payloads cost one mark; a retained byte buffer must not delay header reclamation.
        let reclaimed = self.counters.live_bytes.saturating_sub(live);
        self.pressure_threshold = if live == 0 || reclaimed >= (self.volume / 2).max(2048) {
            4096
        } else {
            traced.max(4096)
        };
        self.volume = 0;
        self.threshold = live.max(MIN_THRESHOLD);
        self.counters.collections += 1;
        self.counters.live_bytes = live;
    }
}

impl Directory {
    const fn new() -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            Self { tables: Vec::new() }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self {
                pages: alloc::collections::BTreeMap::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LINK: [u32; 3] = [0, 1, 0];

    #[test]
    fn cycles_survive_and_overwritten_edges_are_reclaimed() {
        let mut heap = Collector::new();
        // SAFETY: each object contains exactly one initialized pointer; roots are explicit.
        unsafe {
            let a = heap.allocate(core::mem::size_of::<usize>(), LINK.as_ptr());
            let b = heap.allocate(core::mem::size_of::<usize>(), LINK.as_ptr());
            a.cast::<*mut u8>().write(b);
            b.cast::<*mut u8>().write(a);
            heap.collect(&[a]);
            assert_eq!(
                heap.counters().live_bytes,
                2 * core::mem::size_of::<usize>()
            );
            a.cast::<*mut u8>().write(ptr::null_mut());
            heap.collect(&[a]);
            assert_eq!(heap.counters().live_bytes, core::mem::size_of::<usize>());
            assert!(heap.counters().reserved_bytes <= PAGE);
            heap.collect(&[]);
            assert_eq!(heap.counters().live_bytes, 0);
        }
    }

    #[test]
    fn volume_triggers_and_size_classes_reuse_dead_slots() {
        let mut heap = Collector::new();
        // SAFETY: opaque payloads contain no pointers and are dead before each collection.
        unsafe {
            for _ in 0..200_000 {
                heap.allocate(24, ptr::null());
                if heap.due() {
                    heap.collect(&[]);
                }
            }
            heap.collect(&[]);
        }
        assert!(heap.counters().collections > 1);
        assert_eq!(heap.counters().live_bytes, 0);
        assert!(heap.counters().reserved_bytes <= 768 * 1024);
    }

    #[test]
    fn repeated_pointer_slots_trace_without_recursion() {
        const ARRAY: [u32; 3] = [core::mem::size_of::<usize>() as u32, 1, 0];
        let mut heap = Collector::new();
        // SAFETY: the array is zeroed on allocation; all occupied slots hold payload starts.
        unsafe {
            let array = heap.allocate(10_000 * core::mem::size_of::<usize>(), ARRAY.as_ptr());
            for index in 0..10_000 {
                let node = heap.allocate(8, ptr::null());
                array.cast::<*mut u8>().add(index).write(node);
            }
            heap.collect(&[array, core::ptr::dangling_mut::<u8>()]);
            assert_eq!(
                heap.counters().live_bytes,
                10_000 * (8 + core::mem::size_of::<usize>())
            );
            let before = heap.counters().reserved_bytes;
            heap.collect(&[]);
            assert_eq!(heap.counters().live_bytes, 0);
            let after = heap.counters().reserved_bytes;
            assert_eq!(
                before - after,
                (10_000 * core::mem::size_of::<usize>() + HEADER).next_multiple_of(PAGE)
            );
            for _ in 0..10_000 {
                heap.allocate(8, ptr::null());
            }
            assert_eq!(heap.counters().reserved_bytes, after);
        }
    }
    #[test]
    fn grown_pointer_storage_is_zeroed_traced_and_reclaimed() {
        const ARRAY: [u32; 3] = [core::mem::size_of::<usize>() as u32, 1, 0];
        let mut heap = Collector::new();
        // SAFETY: the exclusive pointer array grows within its allocated page. Each pointer
        // is initialized before collection; only the explicit interior root remains live.
        unsafe {
            let array = heap.allocate(600, ARRAY.as_ptr());
            let first = heap.allocate(8, ptr::null());
            array.cast::<*mut u8>().write(first);
            let reserved = heap.counters().reserved_bytes;
            assert!(heap.grow_in_place(array, 800));
            assert_eq!(heap.counters().reserved_bytes, reserved);
            assert!(
                core::slice::from_raw_parts(array.add(600), 200)
                    .iter()
                    .all(|&byte| byte == 0)
            );
            let last = heap.allocate(8, ptr::null());
            array.add(792).cast::<*mut u8>().write(last);
            assert!(heap.contains(array, array.add(799)));
            assert!(!heap.contains(array, array.add(800)));
            heap.collect(&[array.add(799)]);
            assert_eq!(heap.counters().live_bytes, 816);
            assert_eq!(array.cast::<*mut u8>().read(), first);
            assert_eq!(array.add(792).cast::<*mut u8>().read(), last);
            heap.collect(&[]);
            assert_eq!(heap.counters().live_bytes, 0);
        }
    }
}
