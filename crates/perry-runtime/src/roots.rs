//! Precise roots at generated safe points. Callers keep a stack segment while a callee can
//! collect; suspended operations use independent pin tokens until acknowledgement.
use crate::collectible::{Collector, Counters};
use alloc::{boxed::Box, vec::Vec};
use core::{cell::UnsafeCell, ptr};

struct Pin {
    object: *mut u8,
    task: *mut crate::region::Task,
}

struct Roots {
    heap: Collector,
    stack: Vec<*mut u8>,
    pins: Vec<Pin>,
    tasks: Vec<*mut crate::region::Task>,
    free_pins: Vec<u32>,
    snapshot: Vec<*mut u8>,
    globals: Vec<*mut *mut u8>,
    force_collection: bool,
    transfer_copied_bytes: u64,
}

impl Roots {
    const fn new() -> Self {
        Self {
            heap: Collector::new(),
            stack: Vec::new(),
            pins: Vec::new(),
            tasks: Vec::new(),
            free_pins: Vec::new(),
            snapshot: Vec::new(),
            globals: Vec::new(),
            force_collection: false,
            transfer_copied_bytes: 0,
        }
    }

    unsafe fn collect(&mut self) {
        self.snapshot.clear();
        self.snapshot.extend_from_slice(&self.stack);
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
        let index = token.checked_sub(1).expect("invalid pin token") as usize;
        let pinned = self.pins.get_mut(index).expect("invalid pin token");
        assert!(!pinned.object.is_null(), "pin released twice");
        pinned.object = ptr::null_mut();
        pinned.task = ptr::null_mut();
        self.free_pins.push(index as u32);
    }
}

struct Global(UnsafeCell<*mut Roots>);
// SAFETY: component instances are single-threaded; no borrow crosses a runtime entry point.
unsafe impl Sync for Global {}
static ROOTS: Global = Global(UnsafeCell::new(ptr::null_mut()));

#[allow(clippy::mut_from_ref)]
fn roots() -> &'static mut Roots {
    // SAFETY: called synchronously on the component instance's only guest thread.
    unsafe {
        let state = &mut *ROOTS.0.get();
        if state.is_null() {
            *state = Box::into_raw(Box::new(Roots::new()));
        }
        &mut **state
    }
}

/// Allocate collectible storage without running a collection.
///
/// # Safety
/// `shape` obeys [Collector::allocate]'s static shape contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_gc_alloc(size: u32, shape: *const u32) -> *mut u8 {
    // SAFETY: shape validity is the caller's contract.
    unsafe { roots().heap.allocate(size as usize, shape) }
}

/// # Safety
/// `frame` has the nonescaping lifetime established by the MIR frame-storage pass.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_frame_free(frame: *mut u8) {
    unsafe { roots().heap.release_frame(frame) };
}

/// The current stack boundary, used to release exactly one generated activation's roots.
#[unsafe(no_mangle)]
pub extern "C" fn perry_roots_mark() -> u32 {
    u32::try_from(roots().stack.len()).expect("root stack overflow")
}

/// Keep an exact compiled pointer live across calls. Null and static pointers are accepted.
#[unsafe(no_mangle)]
pub extern "C" fn perry_roots_push(object: *mut u8) {
    roots().stack.push(object);
}

/// Release roots established after `mark`, including roots of unwound activations.
#[unsafe(no_mangle)]
pub extern "C" fn perry_roots_reset(mark: u32) {
    let roots = roots();
    assert!(
        mark as usize <= roots.stack.len(),
        "invalid root stack boundary"
    );
    roots.stack.truncate(mark as usize);
}

/// Collect at a generated safe point when allocation volume reaches the live-size threshold.
///
/// # Safety
/// Every live reference is in the root stack, a pin, or reachable through a precise shape.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_gc_check() {
    let roots = roots();
    if roots.force_collection || roots.heap.due() {
        // SAFETY: the caller established a safe point.
        unsafe { roots.collect() };
    }
}

/// Collect at a component return boundary, with output owners pinned for canonical lifting.
///
/// # Safety
/// All surviving compiled values are pinned or registered roots; helpers have completed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_gc_boundary() {
    unsafe { roots().collect() };
}

/// Reclaim an empty instance heap after post-return. A surviving activation, module root or
/// host pin keeps normal volume-based collection in effect instead.
///
/// # Safety
/// The completed task has released its results after lifting/acknowledgement, and all other
/// instance references are represented by the root stack or pins.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_gc_task_end() {
    let roots = roots();
    if roots.stack.is_empty()
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
pub extern "C" fn perry_gc_task_enter() {
    let task = crate::region::perry_task_current();
    assert!(!task.is_null(), "collectible task requires an active task");
    let roots = roots();
    if !roots.tasks.contains(&task) {
        roots.tasks.push(task);
    }
}

/// Detach scheduler roots after all host transfers and cancellation acknowledgements finish.
#[unsafe(no_mangle)]
pub extern "C" fn perry_gc_task_leave() {
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
pub extern "C" fn perry_gc_pin(owner: *mut u8) -> u32 {
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
pub extern "C" fn perry_gc_unpin(token: u32) {
    roots().unpin(token);
}

/// Register a static module cell once; stores replace its root without retaining history.
///
/// # Safety
/// `cell` contains an exact pointer at byte 0 and a zero-initialized registration flag at 12.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_gc_global(cell: *mut u8) {
    // SAFETY: the compiler allocates sixteen static bytes for each module cell.
    unsafe {
        let registered = cell.add(12).cast::<u32>();
        if *registered == 0 {
            roots().globals.push(cell.cast());
            *registered = 1;
        }
    }
}

pub fn counters() -> Counters {
    roots().heap.counters()
}

/// Controls exported only by the instrumented raw-core compiler entry point.
#[unsafe(no_mangle)]
pub extern "C" fn perry_gc_debug(flags: u32) {
    let roots = roots();
    roots.force_collection = flags & 1 != 0;
    roots.heap.set_poison(flags & 2 != 0);
}

/// Allocation count, collection count, live bytes, reserved bytes, copied transfer bytes,
/// active pins and active compiled roots, respectively.
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
        6 => roots.stack.len() as u64,
        _ => panic!("unknown memory counter"),
    }
}

#[cfg(feature = "collectible")]
pub(crate) fn transfer_copied(bytes: u32) {
    roots().transfer_copied_bytes += u64::from(bytes);
}

/// Copy one temporary root segment into collectible owner storage for suspension.
#[unsafe(no_mangle)]
pub extern "C" fn perry_roots_capture(mark: u32) -> *mut u8 {
    const STRIDE: u32 = core::mem::size_of::<*mut u8>() as u32;
    static SHAPE: [u32; 3] = [STRIDE, 1, 0];
    let roots = roots();
    let segment = &roots.stack[mark as usize..];
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
    fn active_pins_survive_other_tasks_and_completed_pins_stop_rooting() {
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
    }
}
