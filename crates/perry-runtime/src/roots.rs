//! Precise roots at generated safe points. Callers keep a stack segment while a callee can
//! collect; suspended operations use independent pin tokens until acknowledgement.
use crate::collectible::Collector;
use crate::instance::instance_state;
use alloc::{boxed::Box, vec::Vec};
#[cfg(target_arch = "wasm32")]
use core::cell::UnsafeCell;
use core::ptr;

struct Pin {
    object: *mut u8,
    task: *mut crate::region::Task,
}

#[cfg(target_arch = "wasm32")]
const _: () = {
    use crate::layout::roots::{CAPACITY, DATA, LENGTH};
    assert!(core::mem::offset_of!(RootStack, data) == DATA as usize);
    assert!(core::mem::offset_of!(RootStack, length) == LENGTH as usize);
    assert!(core::mem::offset_of!(RootStack, capacity) == CAPACITY as usize);
};

/// Stable prefix read by generated Wasm; backing storage is private to the runtime.
#[repr(C)]
pub struct RootStack {
    data: *mut *mut u8,
    length: u32,
    capacity: u32,
}

impl RootStack {
    const fn new() -> Self {
        Self {
            data: ptr::null_mut(),
            length: 0,
            capacity: 0,
        }
    }

    fn reserve(&mut self, additional: u32) {
        let required = self
            .length
            .checked_add(additional)
            .expect("root stack overflow");
        if required <= self.capacity {
            return;
        }
        let capacity = required
            .checked_next_power_of_two()
            .expect("root stack overflow")
            .max(4);
        let layout = Self::layout(capacity);
        // SAFETY: only this descriptor owns its buffer; reallocation never collects.
        let data = unsafe {
            if self.capacity == 0 {
                alloc::alloc::alloc(layout)
            } else {
                alloc::alloc::realloc(self.data.cast(), Self::layout(self.capacity), layout.size())
            }
        };
        if data.is_null() {
            alloc::alloc::handle_alloc_error(layout);
        }
        self.data = data.cast();
        self.capacity = capacity;
    }

    /// The layout of `capacity` slots; an allocated capacity always has one.
    fn layout(capacity: u32) -> core::alloc::Layout {
        core::alloc::Layout::array::<*mut u8>(capacity as usize).expect("root stack overflow")
    }

    fn live(&self) -> &[*mut u8] {
        if self.length == 0 {
            return &[];
        }
        // SAFETY: generated code and `push` initialize every slot before publishing length.
        unsafe { core::slice::from_raw_parts(self.data, self.length as usize) }
    }

    fn push(&mut self, object: *mut u8) {
        self.reserve(1);
        // SAFETY: reserve ensured space; no reference into the buffer survives this call.
        unsafe { self.data.add(self.length as usize).write(object) };
        self.length += 1;
    }
}

impl Drop for RootStack {
    fn drop(&mut self) {
        if self.capacity != 0 {
            // SAFETY: this descriptor exclusively owns this allocation and its layout.
            unsafe { alloc::alloc::dealloc(self.data.cast(), Self::layout(self.capacity)) };
        }
    }
}

#[derive(Default)]
struct GrowthMetrics {
    in_place: u64,
    fallback: u64,
    copied: u64,
}

/// Resize private buffer storage after any host transfer has acknowledged completion.
///
/// # Safety
/// A non-null `old` is an exact live allocation of `old_size` bytes with `initialized` live bytes.
/// The caller owns it exclusively, with no shared slices or outstanding mutable host borrow.
/// `shape` remains valid for the new size. No collection occurs inside this helper.
pub(crate) unsafe fn resize_buffer(
    old: *mut u8,
    old_size: u32,
    new_size: u32,
    initialized: u32,
    shape: *const u32,
) -> *mut u8 {
    assert!(
        initialized <= old_size && old_size <= new_size,
        "buffer resize bounds"
    );
    let roots = roots();
    if !old.is_null() {
        let pinned = roots.pins.len() != roots.free_pins.len()
            && roots.heap.allocation_size(old).is_some_and(|size| {
                roots
                    .pins
                    .iter()
                    .any(|pin| (pin.object as usize).wrapping_sub(old as usize) < size)
            });
        if !pinned && unsafe { roots.heap.grow_in_place(old, new_size as usize) } {
            if let Some(metrics) = &mut roots.growth_metrics {
                metrics.in_place += 1;
            }
            return old;
        }
        if let Some(metrics) = &mut roots.growth_metrics {
            metrics.fallback += 1;
        }
    }
    let new = unsafe { roots.heap.allocate(new_size as usize, shape) };
    if initialized != 0 {
        unsafe { ptr::copy_nonoverlapping(old, new, initialized as usize) };
        if let Some(metrics) = &mut roots.growth_metrics {
            metrics.copied += u64::from(initialized);
        }
    }
    new
}

pub(crate) struct Roots {
    heap: Collector,
    stack: RootStack,
    pins: Vec<Pin>,
    tasks: Vec<*mut crate::region::Task>,
    free_pins: Vec<u32>,
    snapshot: Vec<*mut u8>,
    globals: Vec<*mut *mut u8>,
    force_collection: bool,
    growth_metrics: Option<Box<GrowthMetrics>>,
    transfer_copied_bytes: u64,
}

impl Roots {
    const fn new() -> Self {
        Self {
            heap: Collector::new(),
            stack: RootStack::new(),
            pins: Vec::new(),
            tasks: Vec::new(),
            free_pins: Vec::new(),
            snapshot: Vec::new(),
            globals: Vec::new(),
            force_collection: false,
            growth_metrics: None,
            transfer_copied_bytes: 0,
        }
    }

    unsafe fn collect(&mut self) {
        self.snapshot.clear();
        self.snapshot.extend_from_slice(self.stack.live());
        // SAFETY: compiled module cells are static, pointer-typed storage.
        self.snapshot
            .extend(self.globals.iter().map(|slot| unsafe { **slot }));
        self.snapshot
            .extend(self.pins.iter().map(|p| p.object).filter(|p| !p.is_null()));
        for &task in &self.tasks {
            // SAFETY: tasks register on entry and unregister before their storage is released.
            unsafe { (*task).trace_roots(&mut self.snapshot) };
        }
        // SAFETY: the caller established a safe point and registered all temporary roots.
        #[cfg(any(feature = "collectible", not(target_arch = "wasm32")))]
        unsafe {
            crate::resources::trace(&mut self.snapshot)
        };
        unsafe {
            self.heap.collect_with(&self.snapshot, |heap| {
                #[cfg(any(feature = "collectible", not(target_arch = "wasm32")))]
                crate::resources::finalize(heap);
                #[cfg(all(not(feature = "collectible"), target_arch = "wasm32"))]
                let _ = heap;
            })
        };
        self.snapshot.clear();
    }

    fn pin(&mut self, object: *mut u8) -> u32 {
        let pin = Pin {
            object,
            task: crate::region::perry_task_current(),
        };
        assert!(!object.is_null(), "a pin needs an owner");
        let index = if let Some(index) = self.free_pins.pop() {
            self.pins[index as usize] = pin;
            index
        } else {
            let index = u32::try_from(self.pins.len()).expect("too many pins");
            self.pins.push(pin);
            index
        };
        index + 1
    }

    fn unpin(&mut self, token: u32) {
        // Token 0 wraps to an index no pin table reaches.
        let index = token.wrapping_sub(1);
        let pinned = self
            .pins
            .get_mut(index as usize)
            .expect("invalid pin token");
        assert!(!pinned.object.is_null(), "pin released twice");
        pinned.object = ptr::null_mut();
        pinned.task = ptr::null_mut();
        self.free_pins.push(index);
    }
}

#[cfg(target_arch = "wasm32")]
struct Global(UnsafeCell<Option<Box<Roots>>>);
// SAFETY: component instances are single-threaded; no borrow crosses a runtime entry point.
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for Global {}
instance_state!(ROOTS, roots_state, roots, Option<Box<Roots>>, None);

#[allow(clippy::mut_from_ref)]
fn roots() -> &'static mut Roots {
    // SAFETY: called synchronously on the component instance's only guest thread.
    unsafe {
        let state = &mut *roots_state();
        state.get_or_insert_with(initialize_roots)
    }
}

/// Keep the one-time initializer's stack temporaries out of ordinary root lookups.
#[cold]
fn initialize_roots() -> Box<Roots> {
    Box::new(Roots::new())
}

/// Allocate collectible storage without running a collection.
///
/// # Safety
/// `shape` obeys [Collector::allocate]'s static shape contract.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_gc_alloc(size: u32, shape: *const u32) -> *mut u8 {
    // SAFETY: shape validity is the caller's contract.
    unsafe { roots().heap.allocate(size as usize, shape) }
}

/// # Safety
/// `frame` has the nonescaping lifetime established by the MIR frame-storage pass.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_frame_free(frame: *mut u8) {
    unsafe { roots().heap.release_frame(frame) };
}

/// The descriptor stays at one address even when nested calls grow its backing storage.
#[unsafe(no_mangle)]
pub extern "C" fn perry_roots_state() -> *mut RootStack {
    &raw mut roots().stack
}

/// Grow the root stack without collecting. Generated code publishes slots afterward.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_roots_reserve(additional: u32) {
    roots().stack.reserve(additional);
}

/// The current stack boundary, used to release exactly one generated activation's roots.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_roots_mark() -> u32 {
    roots().stack.length
}

/// Keep an exact compiled pointer live across calls. Null and static pointers are accepted.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_roots_push(object: *mut u8) {
    roots().stack.push(object);
}

/// Release roots established after `mark`, including roots of unwound activations.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_roots_reset(mark: u32) {
    let roots = roots();
    assert!(mark <= roots.stack.length, "invalid root stack boundary");
    roots.stack.length = mark;
}

/// Collect at a generated safe point when allocation volume reaches the live-size threshold.
///
/// # Safety
/// Every live reference is in the root stack, a pin, or reachable through a precise shape.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_gc_check() {
    let roots = roots();
    if roots.force_collection || roots.heap.due() {
        // SAFETY: the caller established a safe point.
        unsafe { roots.collect() };
    }
}

/// Test the existing collection policy before publishing roots for an atomic helper.
/// This function neither allocates nor collects.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_gc_due() -> u32 {
    // SAFETY: the single guest thread owns the state; an absent heap cannot require GC.
    let roots = unsafe { (*roots_state()).as_ref() };
    u32::from(roots.is_some_and(|roots| roots.force_collection || roots.heap.due()))
}

/// Collect at a component return boundary, with output owners pinned for canonical lifting.
///
/// # Safety
/// All surviving compiled values are pinned or registered roots; helpers have completed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_gc_boundary() {
    unsafe { roots().collect() };
}

/// Reclaim an empty instance heap after post-return. A surviving activation, module root or
/// host pin keeps normal volume-based collection in effect instead.
///
/// # Safety
/// The completed task has released its results after lifting/acknowledgement, and all other
/// instance references are represented by the root stack or pins.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_gc_task_end() {
    let roots = roots();
    if roots.stack.length == 0
        && roots.pins.iter().all(|p| p.object.is_null())
        && roots.tasks.is_empty()
        && (roots.force_collection
            || roots.heap.due()
            // SAFETY: registered module cells remain allocated for the instance lifetime.
            || roots.globals.iter().all(|cell| unsafe { (**cell).is_null() }))
    {
        // SAFETY: surviving module cells are included in the collection snapshot.
        unsafe { roots.collect() };
    }
}

/// Register the current task's scheduler roots once, including canonical argument lowering.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_gc_task_enter() {
    let task = crate::region::perry_task_current();
    assert!(!task.is_null(), "collectible task requires an active task");
    let roots = roots();
    if !roots.tasks.contains(&task) {
        roots.tasks.push(task);
    }
}

/// Detach scheduler roots after all host transfers and cancellation acknowledgements finish.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_gc_task_leave() {
    let task = crate::region::perry_task_current();
    #[cfg(any(feature = "collectible", not(target_arch = "wasm32")))]
    unsafe {
        crate::resources::leave(task)
    };
    let roots = roots();
    roots.tasks.retain(|&active| active != task);
    for index in 0..roots.pins.len() {
        let pin = &roots.pins[index];
        if pin.task == task && !pin.object.is_null() {
            roots.unpin(index as u32 + 1);
        }
    }
}

/// Pin an owner independently of the current task's activation stack.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_gc_pin(owner: *mut u8) -> u32 {
    roots().pin(owner)
}

/// Canonical lowering can occur outside a guest task; its owner releases this pin on lift.
#[cfg(feature = "collectible")]
pub(crate) fn pin_external(owner: *mut u8) -> u32 {
    let roots = roots();
    let token = roots.pin(owner);
    roots.pins[token as usize - 1].task = ptr::null_mut();
    token
}

/// Release a pin after the host acknowledged completion or cancellation.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_gc_unpin(token: u32) {
    roots().unpin(token);
}

/// Register a static module cell once; stores replace its root without retaining history.
///
/// # Safety
/// `cell` contains an exact pointer at byte 0 and a zero-initialized registration flag at 12.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_gc_global(cell: *mut u8) {
    // SAFETY: the compiler allocates sixteen static bytes for each module cell.
    unsafe {
        let registered = cell.add(12).cast::<u32>();
        if *registered == 0 {
            roots().globals.push(cell.cast());
            *registered = 1;
        }
    }
}

#[cfg(all(test, feature = "collectible"))]
pub(crate) fn counters() -> crate::collectible::Counters {
    roots().heap.counters()
}

/// Test controls: force collection (1), poison reclamation (2), enable growth metrics (4).
#[unsafe(no_mangle)]
pub extern "C" fn perry_gc_debug(flags: u32) {
    let roots = roots();
    roots.force_collection = flags & 1 != 0;
    if flags & 4 != 0 && roots.growth_metrics.is_none() {
        roots.growth_metrics = Some(Box::default());
    }
    roots.heap.set_poison(flags & 2 != 0);
}

/// Indices 0–6: allocations, collections, live bytes, reserved bytes, transfer-copy bytes,
/// active pins and roots. Compiled wrappers supply root publications at 7; indices 8–10
/// are in-place growths, copy fallbacks and growth-copy bytes. Index 11 counts live
/// WebAssembly address-lookup tables (zero for the native ordered-map implementation).
#[unsafe(no_mangle)]
pub extern "C" fn perry_gc_counter(index: u32) -> u64 {
    let roots = roots();
    let counters = roots.heap.counters();
    match index {
        0 => counters.allocations,
        1 => counters.collections,
        2 => counters.live_bytes as u64,
        3 => counters.reserved_bytes as u64,
        4 => roots.transfer_copied_bytes,
        5 => roots.pins.iter().filter(|p| !p.object.is_null()).count() as u64,
        6 => u64::from(roots.stack.length),
        8 => roots.growth_metrics.as_ref().map_or(0, |m| m.in_place),
        9 => roots.growth_metrics.as_ref().map_or(0, |m| m.fallback),
        10 => roots.growth_metrics.as_ref().map_or(0, |m| m.copied),
        11 => roots.heap.directory_tables() as u64,
        _ => panic!("unknown memory counter"),
    }
}

#[cfg(feature = "collectible")]
pub(crate) fn transfer_copied(bytes: u32) {
    roots().transfer_copied_bytes += u64::from(bytes);
}

/// Copy one temporary root segment into collectible owner storage for suspension.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_roots_capture(mark: u32) -> *mut u8 {
    const STRIDE: u32 = core::mem::size_of::<*mut u8>() as u32;
    static SHAPE: [u32; 3] = [STRIDE, 1, 0];
    let roots = roots();
    let segment = &roots.stack.live()[mark as usize..];
    let bytes = core::mem::size_of_val(segment);
    // SAFETY: exact pointer slots, no internal collection, and disjoint newly allocated storage.
    unsafe {
        let owner = roots
            .heap
            .allocate(bytes.max(STRIDE as usize), SHAPE.as_ptr());
        ptr::copy_nonoverlapping(segment.as_ptr().cast::<u8>(), owner, bytes);
        owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interior_borrows_prevent_growth_until_release() {
        crate::RuntimeInstance::default().enter(|| {
            // SAFETY: all allocations are opaque, roots stay pinned across collection, and
            // each resize receives its live allocation size and initialized prefix.
            unsafe {
                for offset in [0, 599] {
                    let old = perry_gc_alloc(600, ptr::null());
                    ptr::write_bytes(old, 0x5a, 600);
                    let borrow = perry_gc_pin(old.add(offset));
                    let copy = resize_buffer(old, 600, 800, 600, ptr::null());
                    assert_ne!(copy, old);
                    let copied = perry_gc_pin(copy);
                    roots().collect();
                    assert_eq!(core::slice::from_raw_parts(old, 600), &[0x5a; 600]);
                    assert_eq!(core::slice::from_raw_parts(copy, 600), &[0x5a; 600]);
                    perry_gc_unpin(borrow);
                    perry_gc_unpin(copied);
                    assert_eq!(resize_buffer(copy, 800, 900, 600, ptr::null()), copy);
                    roots().collect();
                }
            }
        });
    }

    #[test]
    fn active_pins_survive_other_tasks_and_completed_pins_stop_rooting() {
        crate::RuntimeInstance::default().enter(|| {
            let mut roots = Roots::new();
            // SAFETY: no object has pointer fields; only registered roots are used after collect.
            unsafe {
                let first = roots.heap.allocate(128, ptr::null());
                let second = roots.heap.allocate(256, ptr::null());
                let pin_first = roots.pin(first);
                let pin_second = roots.pin(second);
                roots.collect();
                assert_eq!(roots.heap.counters().live_bytes, 384);
                roots.unpin(pin_first);
                roots.collect();
                assert_eq!(roots.heap.counters().live_bytes, 256);
                assert!(roots.snapshot.is_empty());
                let next = roots.heap.allocate(128, ptr::null());
                assert_eq!(roots.pin(next), pin_first);
                roots.unpin(pin_first);
                roots.unpin(pin_second);
                roots.collect();
                assert_eq!(roots.heap.counters().live_bytes, 0);
            }
        });
    }
}
