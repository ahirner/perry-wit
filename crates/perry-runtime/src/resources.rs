//! Opaque resource owners and transactional canonical transfers. Active transfers root
//! descriptors, while the weak registry queues host destruction after marking finishes.
use crate::{collectible::Collector, instance::instance_state, region::Task};
use alloc::{boxed::Box, vec::Vec};
#[cfg(target_arch = "wasm32")]
use core::cell::UnsafeCell;
use core::ptr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Owned,
    Moving(*mut Scope),
    Borrowed {
        task: *mut Task,
        representation: bool,
    },
    Consumed,
}

#[repr(C)]
pub(crate) struct Resource {
    kind: u32,
    handle: u32,
    state: State,
    borrows: u32,
}

impl Resource {
    /// The host destruction this descriptor owes.
    fn drop_handle(&self) -> DropHandle {
        DropHandle {
            kind: self.kind,
            handle: self.handle,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DropHandle {
    kind: u32,
    handle: u32,
}

enum Movement {
    Move(*mut Resource),
    Borrow(*mut Resource),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Preparing,
    Committed,
}

pub(crate) struct Scope {
    phase: Phase,
    task: *mut Task,
    movements: Vec<Movement>,
}

pub(crate) struct Registry {
    descriptors: Vec<*mut Resource>,
    #[expect(
        clippy::vec_box,
        reason = "ABI scope pointers must survive vector growth"
    )]
    scopes: Vec<Box<Scope>>,
    drops: Vec<DropHandle>,
}

impl Registry {
    pub(crate) const fn new() -> Self {
        Self {
            descriptors: Vec::new(),
            scopes: Vec::new(),
            drops: Vec::new(),
        }
    }

    unsafe fn trace(&self, roots: &mut Vec<*mut u8>) {
        for scope in &self.scopes {
            for movement in &scope.movements {
                match *movement {
                    Movement::Move(owner) if scope.phase == Phase::Preparing => {
                        roots.push(owner.cast())
                    }
                    Movement::Borrow(owner) => roots.push(owner.cast()),
                    _ => {}
                }
            }
        }
    }

    unsafe fn finalize(&mut self, heap: &Collector) {
        self.descriptors.retain(|&owner| {
            if heap.is_marked(owner.cast()) {
                return true;
            }
            // SAFETY: finalization runs after marking and before dead storage can be swept.
            let owner = unsafe { &*owner };
            assert_eq!(owner.borrows, 0, "an active resource borrow was not rooted");
            match owner.state {
                State::Owned
                | State::Borrowed {
                    representation: false,
                    ..
                } => {
                    self.drops.push(owner.drop_handle());
                }
                State::Moving(_) => panic!("a preparing resource transfer was not rooted"),
                _ => {}
            }
            false
        });
    }

    unsafe fn leave(&mut self, task: *mut Task) {
        assert!(
            self.scopes.iter().all(|scope| scope.task != task),
            "resource transfer lacks acknowledgement"
        );
        for &owner in &self.descriptors {
            let owner = unsafe { &mut *owner };
            if let State::Borrowed {
                task: lender,
                representation,
            } = owner.state
                && lender == task
            {
                assert_eq!(
                    owner.borrows, 0,
                    "resource borrowed beyond task acknowledgement"
                );
                owner.state = State::Consumed;
                if !representation {
                    self.drops.push(owner.drop_handle());
                }
            }
        }
    }

    fn scope(&mut self, task: *mut Task) -> *mut Scope {
        let mut scope = Box::new(Scope {
            phase: Phase::Preparing,
            task,
            movements: Vec::new(),
        });
        let pointer = ptr::from_mut(&mut *scope);
        self.scopes.push(scope);
        pointer
    }

    unsafe fn finish(&mut self, scope: *mut Scope) {
        let index = self
            .scopes
            .iter()
            .position(|active| ptr::eq(&**active, scope))
            .expect("transfer finished twice");
        let scope = self.scopes.swap_remove(index);
        for movement in scope.movements {
            match movement {
                Movement::Move(owner) if scope.phase == Phase::Preparing => unsafe {
                    (*owner).state = State::Owned
                },
                Movement::Borrow(owner) => unsafe { (*owner).borrows -= 1 },
                _ => {}
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
struct Global(UnsafeCell<Registry>);
// SAFETY: one guest thread; no borrow crosses a host call or allocation safe point.
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for Global {}
instance_state!(
    REGISTRY,
    registry_state,
    registry,
    Registry,
    Registry::new()
);
#[allow(clippy::mut_from_ref)]
fn registry() -> &'static mut Registry {
    unsafe { &mut *registry_state() }
}

pub(crate) unsafe fn trace(roots: &mut Vec<*mut u8>) {
    unsafe { registry().trace(roots) };
}
pub(crate) unsafe fn finalize(heap: &Collector) {
    unsafe { registry().finalize(heap) };
}
pub(crate) unsafe fn leave(task: *mut Task) {
    unsafe { registry().leave(task) };
}

fn invalid() {
    crate::exception::perry_raise(crate::exception::TYPE_ERROR, ptr::null_mut());
}
fn readable(owner: &Resource, task: *mut Task) -> bool {
    matches!(owner.state, State::Owned)
        || matches!(owner.state, State::Borrowed { task: lender, .. } if lender == task)
}

/// Adopt a canonical handle. Mode: owned (0), borrowed handle (1), borrowed representation (2).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_resource_new(kind: u32, handle: u32, mode: u32) -> *mut Resource {
    let state = match mode {
        0 => State::Owned,
        1 | 2 => State::Borrowed {
            task: crate::region::perry_task_current(),
            representation: mode == 2,
        },
        _ => panic!("invalid resource mode"),
    };
    // SAFETY: a descriptor contains no collectible references; scopes trace their own owners.
    let owner = unsafe {
        crate::roots::perry_gc_alloc(core::mem::size_of::<Resource>() as u32, ptr::null())
            .cast::<Resource>()
    };
    unsafe {
        owner.write(Resource {
            kind,
            handle,
            state,
            borrows: 0,
        })
    };
    registry().descriptors.push(owner);
    owner
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_resource_scope_new() -> *mut Scope {
    registry().scope(crate::region::perry_task_current())
}

/// # Safety
/// Scope is preparing and owner is a live descriptor of the declared resource kind.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_resource_lower(
    scope: *mut Scope,
    owner: *mut Resource,
    owned: u32,
) -> u32 {
    let scope_ref = unsafe { &mut *scope };
    let owner_ref = unsafe { &mut *owner };
    assert!(
        scope_ref.phase == Phase::Preparing,
        "resource transfer already committed"
    );
    if owned != 0 {
        if owner_ref.state != State::Owned || owner_ref.borrows != 0 {
            invalid();
            return 0;
        }
        owner_ref.state = State::Moving(scope);
        scope_ref.movements.push(Movement::Move(owner));
    } else {
        if !readable(owner_ref, scope_ref.task) {
            invalid();
            return 0;
        }
        owner_ref.borrows = owner_ref
            .borrows
            .checked_add(1)
            .expect("too many resource borrows");
        scope_ref.movements.push(Movement::Borrow(owner));
    }
    owner_ref.handle
}

/// # Safety
/// Owner is a live descriptor, and access is checked against its task and ownership state.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_resource_handle(owner: *mut Resource) -> u32 {
    let owner = unsafe { &*owner };
    if !readable(owner, crate::region::perry_task_current()) {
        invalid();
        return 0;
    }
    owner.handle
}

/// # Safety
/// Owner is a live descriptor.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_resource_is_rep(owner: *mut Resource) -> u32 {
    u32::from(matches!(
        unsafe { (*owner).state },
        State::Borrowed {
            representation: true,
            ..
        }
    ))
}

/// # Safety
/// Owner is a live descriptor of the declared kind.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_resource_drop(owner: *mut Resource) {
    let owner = unsafe { &mut *owner };
    if owner.state != State::Owned || owner.borrows != 0 {
        invalid();
        return;
    }
    owner.state = State::Consumed;
    registry().drops.push(owner.drop_handle());
}

/// # Safety
/// Every argument validated successfully and scope has not been committed.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_resource_scope_commit(scope: *mut Scope) {
    let scope_ref = unsafe { &mut *scope };
    assert!(
        scope_ref.phase == Phase::Preparing,
        "resource transfer already committed"
    );
    for movement in &scope_ref.movements {
        if let Movement::Move(owner) = *movement {
            let owner = unsafe { &mut *owner };
            assert_eq!(owner.state, State::Moving(scope));
            owner.state = State::Consumed;
        }
    }
    scope_ref.phase = Phase::Committed;
}

/// # Safety
/// Scope is live, and acknowledgement has arrived if it was committed to a host call.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_resource_scope_finish(scope: *mut Scope) {
    unsafe { registry().finish(scope) };
}

/// Pop the next queued resource destruction, returning `0` when none remains or packing
/// `kind + 1` in the high bits and the handle in the low bits.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_resource_drop_next() -> u64 {
    registry().drops.pop().map_or(0, |drop| {
        (u64::from(drop.kind) + 1) << 32 | u64::from(drop.handle)
    })
}

/// End inbound borrows while host destructors may still be called, before canonical return.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_resource_task_return() {
    unsafe { registry().leave(crate::region::perry_task_current()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{exception::perry_status, region::*, roots::*};

    #[test]
    fn resource_transfers_rollback_pin_and_expire_exactly_once() {
        crate::RuntimeInstance::default().enter(|| {
            let task = perry_task_enter();
            perry_gc_task_enter();
            perry_gc_debug(3);
            let owner = perry_resource_new(2, 71, 0);
            let pin = perry_gc_pin(owner.cast());
            // A duplicate own argument fails without consuming the earlier alias.
            unsafe {
                let scope = perry_resource_scope_new();
                assert_eq!(perry_resource_lower(scope, owner, 1), 71);
                perry_resource_lower(scope, owner, 1);
                assert_eq!(perry_status(), crate::exception::TYPE_ERROR);
                perry_resource_scope_finish(scope);
                assert_eq!(perry_resource_handle(owner), 71);
                assert_eq!(perry_status(), 0);
                // A borrowed host call keeps the owner live after its last compiled reference dies.
                let scope = perry_resource_scope_new();
                perry_resource_lower(scope, owner, 0);
                perry_resource_scope_commit(scope);
                perry_gc_unpin(pin);
                perry_gc_check();
                perry_resource_drop(owner);
                assert_eq!(perry_status(), crate::exception::TYPE_ERROR);
                assert_eq!(perry_resource_drop_next(), 0);
                perry_resource_scope_finish(scope);
                perry_gc_check();
                assert_eq!(perry_resource_drop_next(), 3u64 << 32 | 71);
                perry_gc_check();
                assert_eq!(perry_resource_drop_next(), 0);
                // Ownership transfer invalidates every local alias and suppresses GC destruction.
                let moved = perry_resource_new(2, 72, 0);
                let pin = perry_gc_pin(moved.cast());
                let scope = perry_resource_scope_new();
                perry_resource_lower(scope, moved, 1);
                perry_resource_scope_commit(scope);
                perry_resource_handle(moved);
                assert_eq!(perry_status(), crate::exception::TYPE_ERROR);
                perry_gc_unpin(pin);
                perry_gc_check();
                perry_resource_scope_finish(scope);
                assert_eq!(perry_resource_drop_next(), 0);
                // End of the lending task invalidates saved borrowed aliases, even when pinned.
                let borrowed = perry_resource_new(3, 81, 1);
                let representation = perry_resource_new(4, 91, 2);
                let other = {
                    perry_task_suspend();
                    perry_task_enter()
                };
                perry_gc_task_enter();
                let pin_borrow = perry_gc_pin(borrowed.cast());
                let pin_rep = perry_gc_pin(representation.cast());
                perry_resource_handle(borrowed);
                assert_eq!(perry_status(), crate::exception::TYPE_ERROR);
                perry_task_resume(task);
                perry_gc_task_leave();
                perry_task_free();
                assert_eq!(perry_resource_drop_next(), 4u64 << 32 | 81);
                assert_eq!(perry_resource_drop_next(), 0);
                perry_task_resume(other);
                perry_resource_handle(borrowed);
                assert_eq!(perry_status(), crate::exception::TYPE_ERROR);
                perry_resource_handle(representation);
                assert_eq!(perry_status(), crate::exception::TYPE_ERROR);
                perry_gc_unpin(pin_borrow);
                perry_gc_unpin(pin_rep);
                perry_gc_task_leave();
                perry_task_free();
                perry_gc_task_end();
                assert_eq!(perry_resource_drop_next(), 0);
            }
            perry_gc_debug(0);
        });
    }
}
