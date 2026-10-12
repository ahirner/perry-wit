//! Promises of the current task (design decision D5).
//!
//! Layout, read inline by compiled code: `state: u32` at 0 (`PENDING`, `FULFILLED`,
//! `REJECTED`), `status: i32` at 4 (the exception code of a rejection), the settled value's
//! 8-byte slot at 8 (`f64` bits or a 32-bit reference; a rejection's exception object).
//! Compiled code checks `state` before suspending, so awaiting a settled promise continues
//! inline; otherwise observers share a private P3 future whose completion dispatches that
//! promise's registered frames.
use crate::{host, region::current};
use core::ptr;

pub const STATE: u32 = 0;
pub const STATUS: u32 = 4;
pub const VALUE: u32 = 8;

const PENDING: u32 = 0;
pub const FULFILLED: u32 = 1;
pub const REJECTED: u32 = 2;

#[repr(C)]
struct Promise {
    state: u32,
    status: i32,
    value: u64,
    waiters: *mut Observer,
    waiters_tail: *mut Observer,
    signal: Signal,
    owner: *mut u8,
    references: bool,
    cancellation_owned: bool,
}

/// A pending promise in the current task.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_new() -> *mut u8 {
    perry_promise_new_typed(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_new_typed(references: u32) -> *mut u8 {
    #[cfg(feature = "collectible")]
    {
        perry_promise_new_collectible(references)
    }
    #[cfg(not(feature = "collectible"))]
    {
        let promise = current().allocate(core::mem::size_of::<Promise>()) as *mut Promise;
        // SAFETY: freshly allocated.
        unsafe { pending(promise, references) }
    }
}

/// A typed promise whose settled reference payload is traced precisely.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_new_collectible(references: u32) -> *mut u8 {
    static SHAPE: [u32; 3] = [0, 1, core::mem::offset_of!(Promise, owner) as u32];
    // SAFETY: the static shape describes the owner field, independently of value-slot bits.
    let promise = unsafe {
        crate::roots::perry_gc_alloc(core::mem::size_of::<Promise>() as u32, SHAPE.as_ptr())
    } as *mut Promise;
    // SAFETY: freshly allocated; allocation never collects within this helper.
    unsafe { pending(promise, references) }
}

/// Initialize fresh storage as a pending promise; `references` types its settled value.
///
/// # Safety
/// `promise` points to writable storage for a [Promise].
unsafe fn pending(promise: *mut Promise, references: u32) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        promise.write(Promise {
            state: PENDING,
            status: 0,
            value: 0,
            waiters: ptr::null_mut(),
            waiters_tail: ptr::null_mut(),
            signal: Signal::new(promise.cast(), SignalKind::Promise),
            owner: ptr::null_mut(),
            references: references != 0,
            cancellation_owned: false,
        })
    };
    promise.cast()
}

/// Whether a fulfilled `promise` holds a traced reference rather than number bits.
pub(crate) unsafe fn has_reference_value(promise: *mut u8) -> bool {
    // SAFETY: caller supplies a live promise.
    unsafe { (*(promise as *const Promise)).references }
}

/// `promise` made pending again if it settled and nothing waits on it, otherwise a new
/// promise. A leased operation (`perry_codegen::lease`) passes its previous promise, which no
/// code can observe any more.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_reuse(promise: *mut u8) -> *mut u8 {
    let reusable = promise as *mut Promise;
    // SAFETY: a non-null argument is a live promise of the current task.
    unsafe {
        if reusable.is_null()
            || (*reusable).state == PENDING
            || !(*reusable).waiters.is_null()
            || (*reusable).signal.state != SignalState::Dormant
        {
            return perry_promise_new_typed(1);
        }
        (*reusable).state = PENDING;
        (*reusable).status = 0;
        (*reusable).value = 0;
        (*reusable).owner = ptr::null_mut();
        (*reusable).cancellation_owned = false;
    }
    promise
}

fn settle(promise: *mut u8, state: u32, status: i32, value: u64) {
    let promise = promise as *mut Promise;
    // SAFETY: compiled code passes live promises of the current task.
    unsafe {
        if (*promise).state != PENDING {
            return;
        }
        (*promise).state = state;
        (*promise).status = status;
        (*promise).value = value;
        (*promise).owner = if state == REJECTED || (*promise).references {
            value as usize as *mut u8
        } else {
            ptr::null_mut()
        };
        let mut observer = (*promise).waiters;
        while !observer.is_null() {
            let next = (*observer).promise_next;
            let frame = (*observer).frame;
            if *frame.cast::<u32>() == COMBINATOR_KIND {
                unlink(observer);
                accept_completion(&mut *frame.cast::<Combinator>(), promise);
            }
            observer = next;
        }
        if !(*promise).waiters.is_null() {
            (*promise).signal.fire();
        }
    }
}

/// Fulfil `promise` with the slot bits `value` and signal its observers.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_resolve(promise: *mut u8, value: u64) {
    settle(promise, FULFILLED, 0, value);
}

/// Reject `promise` with exception code `status`, taking the pending exception object into
/// its value slot, and signal its observers.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_reject(promise: *mut u8, status: i32) {
    let exception = core::mem::replace(&mut current().exception, ptr::null_mut());
    settle(promise, REJECTED, status, exception as usize as u64);
}

/// `await` of the rejected `promise`: make its exception pending again; returns its status.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_rethrow(promise: *mut u8) -> i32 {
    let promise = promise as *mut Promise;
    // SAFETY: compiled code passes a live, rejected promise.
    unsafe {
        current().exception = (*promise).value as usize as *mut u8;
        (*promise).status
    }
}

/// Pending registrations belong to both their promise and their owning task. Generated
/// frames embed one at `layout::async_frame::OBSERVER`, combinators after their header.
#[repr(C)]
struct Observer {
    frame: *mut u8,
    promise: *mut Promise,
    promise_next: *mut Observer,
    promise_previous: *mut Observer,
    task_next: *mut Observer,
    task_previous: *mut Observer,
}

#[cfg(target_arch = "wasm32")]
const _: () =
    assert!(core::mem::size_of::<Observer>() <= crate::layout::async_frame::OBSERVER_SIZE as usize);

/// The task's intrusive list of active observers, traced as GC roots and drained on cancel.
pub(crate) struct Observers {
    head: *mut Observer,
}
impl Observers {
    pub(crate) const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
        }
    }
    unsafe fn insert(&mut self, observer: *mut Observer) {
        unsafe {
            (*observer).task_next = self.head;
            if !self.head.is_null() {
                (*self.head).task_previous = observer;
            }
            self.head = observer;
        }
    }
    unsafe fn remove(&mut self, observer: *mut Observer) {
        unsafe {
            let previous = (*observer).task_previous;
            let next = (*observer).task_next;
            if previous.is_null() {
                self.head = next;
            } else {
                (*previous).task_next = next;
            }
            if !next.is_null() {
                (*next).task_previous = previous;
            }
            (*observer).task_previous = ptr::null_mut();
            (*observer).task_next = ptr::null_mut();
        }
    }
    unsafe fn visit(&self, mut visit: impl FnMut(*mut Observer)) {
        let mut observer = self.head;
        while !observer.is_null() {
            visit(observer);
            observer = unsafe { (*observer).task_next };
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_null()
    }
    pub(crate) unsafe fn trace(&self, roots: &mut alloc::vec::Vec<*mut u8>) {
        unsafe {
            self.visit(|observer| roots.extend([(*observer).frame, (*observer).promise.cast()]));
        }
    }
}

/// Life cycle of a [Signal]'s native future, from creation to dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum SignalState {
    /// No native future; the signal may be armed or its owner reused.
    Dormant,
    /// Read pending, awaiting `fire`.
    Armed,
    /// Written; the task's waitable set will report the event.
    Signalled,
    /// The event arrived and the dispatcher is draining observers.
    Dispatching,
    /// A combinator's frame was handed to the dispatcher.
    Delivered,
}
/// What the [Signal]'s owner is, which selects the fan-out of [perry_promise_next].
#[derive(Clone, Copy)]
#[repr(u8)]
enum SignalKind {
    Promise,
    Combinator,
}

/// One native completion for a promise's fan-out or a combinator's ready result.
#[repr(C)]
struct Signal {
    owner: *mut u8,
    bucket_next: *mut Signal,
    readable: u32,
    writable: u32,
    kind: SignalKind,
    state: SignalState,
}
impl Signal {
    fn new(owner: *mut u8, kind: SignalKind) -> Self {
        Self {
            owner,
            bucket_next: ptr::null_mut(),
            readable: 0,
            writable: 0,
            kind,
            state: SignalState::Dormant,
        }
    }
    /// Open a fresh native future for a dormant signal and start reading it.
    unsafe fn arm(&mut self) {
        // `assert!` rather than `assert_eq!`: the trap needs no `Debug` formatting code.
        assert!(
            self.state == SignalState::Dormant,
            "only a dormant signal arms"
        );
        unsafe {
            let pair = host::reaction_new();
            self.readable = pair as u32;
            self.writable = (pair >> 32) as u32;
            self.state = SignalState::Armed;
            current().signals.insert(self);
            // Nothing was written yet, so the read stays pending until `fire`.
            let read = host::reaction_read(self.readable, ptr::null_mut());
            assert!(read == host::BLOCKED, "an unwritten future read blocks");
            crate::io::perry_task_join(self.readable);
        }
    }
    /// Complete an armed signal's future; its event follows through the waitable set.
    unsafe fn fire(&mut self) {
        assert!(
            self.state == SignalState::Armed,
            "only an armed signal fires"
        );
        unsafe {
            self.state = SignalState::Signalled;
            // The pending read accepts the unit payload at once.
            let written = host::reaction_write(self.writable, ptr::null());
            assert!(written == 0, "the pending read accepts the write at once");
            host::reaction_drop_writable(self.writable);
        }
    }
    /// Release the native future and unregister the signal, leaving it dormant for reuse.
    unsafe fn close(&mut self) {
        unsafe {
            if self.state == SignalState::Armed {
                self.fire();
            }
            if self.state == SignalState::Signalled {
                host::waitable_join(self.readable, 0);
                host::reaction_cancel_read(self.readable);
            }
            host::reaction_drop_readable(self.readable);
            let removed = current().signals.remove(self);
            assert!(removed, "an open completion is registered");
            self.state = SignalState::Dormant;
        }
    }
}

/// Handle lookup and GC ownership for native completions; entries have no runnable order.
pub(crate) struct Signals {
    buckets: &'static mut [*mut Signal],
    len: usize,
}
impl Signals {
    pub(crate) const fn new() -> Self {
        Self {
            buckets: &mut [],
            len: 0,
        }
    }
    unsafe fn insert_bucket(&mut self, signal: *mut Signal) {
        unsafe {
            let bucket = &mut self.buckets[(*signal).readable as usize & (self.buckets.len() - 1)];
            (*signal).bucket_next = *bucket;
            *bucket = signal;
            self.len += 1;
        }
    }
    unsafe fn insert(&mut self, signal: *mut Signal) {
        unsafe {
            if self.len == self.buckets.len() {
                let capacity = (self.len * 2).max(8);
                let buckets = current()
                    .allocate(capacity * core::mem::size_of::<*mut Signal>())
                    .cast::<*mut Signal>();
                ptr::write_bytes(buckets, 0, capacity);
                // The task owns this initialized bucket array until it drops the registry.
                let buckets = core::slice::from_raw_parts_mut(buckets, capacity);
                let old = core::mem::replace(&mut self.buckets, buckets);
                self.len = 0;
                for &mut head in old {
                    let mut entry = head;
                    while !entry.is_null() {
                        let next = (*entry).bucket_next;
                        self.insert_bucket(entry);
                        entry = next;
                    }
                }
            }
            self.insert_bucket(signal);
        }
    }
    fn get(&self, handle: u32) -> *mut Signal {
        if self.is_empty() {
            return ptr::null_mut();
        }
        let mut entry = self.buckets[handle as usize & (self.buckets.len() - 1)];
        // SAFETY: registered signals belong to live rooted owners until removal.
        while let Some(signal) = unsafe { entry.as_ref() } {
            if signal.readable == handle {
                return entry;
            }
            entry = signal.bucket_next;
        }
        ptr::null_mut()
    }

    fn remove(&mut self, signal: &Signal) -> bool {
        if self.is_empty() {
            return false;
        }
        let capacity = self.buckets.len();
        let mut link = &mut self.buckets[signal.readable as usize & (capacity - 1)];
        while !link.is_null() {
            if core::ptr::eq(*link, signal) {
                *link = signal.bucket_next;
                self.len -= 1;
                return true;
            }
            // SAFETY: preceding live nodes are distinct from the signal being closed;
            // only this registry mutates their bucket links.
            link = unsafe { &mut (**link).bucket_next };
        }
        false
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn trace(&self, roots: &mut alloc::vec::Vec<*mut u8>) {
        for &head in self.buckets.iter() {
            let mut entry = head;
            // SAFETY: the registry retains every owner until its event is acknowledged.
            while let Some(signal) = unsafe { entry.as_ref() } {
                roots.push(signal.owner);
                entry = signal.bucket_next;
            }
        }
    }
}

/// Detach a registered observer from both its promise's and its task's list.
unsafe fn unlink(observer: *mut Observer) {
    unsafe {
        let promise = (*observer).promise;
        let previous = (*observer).promise_previous;
        let next = (*observer).promise_next;
        if previous.is_null() {
            (*promise).waiters = next;
        } else {
            (*previous).promise_next = next;
        }
        if next.is_null() {
            (*promise).waiters_tail = previous;
        } else {
            (*next).promise_previous = previous;
        }
        (*observer).promise_next = ptr::null_mut();
        (*observer).promise_previous = ptr::null_mut();
        current().observers.remove(observer);
    }
}

/// Register a pending await in its frame; observers of one promise share a P3 completion.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_wait(promise: *mut u8, frame: *mut u8, storage: *mut u8) {
    let promise = promise.cast::<Promise>();
    let observer = storage.cast::<Observer>();
    // SAFETY: storage belongs to a suspended frame and has no active registration.
    unsafe {
        // Compiled code and combinators only wait on pending promises.
        assert!(
            (*promise).state == PENDING,
            "only pending promises are awaited"
        );
        observer.write(Observer {
            frame,
            promise,
            promise_next: ptr::null_mut(),
            promise_previous: (*promise).waiters_tail,
            task_next: ptr::null_mut(),
            task_previous: ptr::null_mut(),
        });
        if (*promise).waiters_tail.is_null() {
            (*promise).waiters = observer;
        } else {
            (*(*promise).waiters_tail).promise_next = observer;
        }
        (*promise).waiters_tail = observer;
        current().observers.insert(observer);
        // Combinators count settlements synchronously; generated frames share the signal,
        // which the first waiter arms.
        if *frame.cast::<u32>() != COMBINATOR_KIND
            && (*promise).signal.state == SignalState::Dormant
        {
            (*promise).signal.arm();
        }
    }
}

/// Begin the fan-out selected by one native P3 completion event.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_event(event: u32, handle: u32) -> *mut u8 {
    if event != host::EVENT_FUTURE_READ {
        return ptr::null_mut();
    }
    // SAFETY: registered signals live in rooted owners until they close.
    unsafe {
        let signal = current().signals.get(handle);
        if signal.is_null() || (*signal).state != SignalState::Signalled {
            return ptr::null_mut();
        }
        (*signal).state = SignalState::Dispatching;
        signal.cast()
    }
}

/// One observer of this completion, retaining its owner until the fan-out is exhausted.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_next(completion: *mut u8) -> *mut u8 {
    // SAFETY: `completion` came from [perry_promise_event] and stays registered until closed.
    unsafe {
        let signal = &mut *completion.cast::<Signal>();
        match signal.kind {
            SignalKind::Promise => {
                let promise = signal.owner.cast::<Promise>();
                let observer = (*promise).waiters;
                if !observer.is_null() {
                    let frame = (*observer).frame;
                    unlink(observer);
                    return frame;
                }
            }
            SignalKind::Combinator if signal.state == SignalState::Dispatching => {
                signal.state = SignalState::Delivered;
                return signal.owner;
            }
            SignalKind::Combinator => {}
        }
        signal.close();
        ptr::null_mut()
    }
}

/// Completed combinators own cancellation of losing operations, including nested awaits.
pub(crate) fn prepare_cancellation() {
    // SAFETY: active observers, their frames and promises are live members of this task.
    unsafe {
        current().observers.visit(|observer| {
            let frame = (*observer).frame;
            if *frame.cast::<u32>() == COMBINATOR_KIND
                && (*(*(frame.cast::<Combinator>())).result.cast::<Promise>()).state != PENDING
            {
                (*(*observer).promise).cancellation_owned = true;
            }
        });
        loop {
            let mut changed = false;
            current().observers.visit(|observer| {
                let frame = (*observer).frame;
                if *frame.cast::<u32>() != COMBINATOR_KIND {
                    // The generated frame's result promise is the third Wasm32 word.
                    #[cfg(target_arch = "wasm32")]
                    let result = *frame
                        .add(crate::layout::async_frame::PROMISE as usize)
                        .cast::<*mut Promise>();
                    #[cfg(not(target_arch = "wasm32"))]
                    let result: *mut Promise = ptr::null_mut();
                    if !result.is_null()
                        && (*result).cancellation_owned
                        && !(*(*observer).promise).cancellation_owned
                    {
                        (*(*observer).promise).cancellation_owned = true;
                        changed = true;
                    }
                }
            });
            if !changed {
                break;
            }
        }
    }
}

/// Whether a settled combinator, marked by [prepare_cancellation], cancels `promise`'s operation.
pub(crate) unsafe fn cancellation_owned(promise: *mut u8) -> bool {
    unsafe { (*promise.cast::<Promise>()).cancellation_owned }
}

/// Close every native completion and drop every registration of the current task.
pub(crate) fn cancel_observers() {
    // SAFETY: signal cancellation acknowledges native reads before their owners are released.
    unsafe {
        for bucket in 0..current().signals.buckets.len() {
            while !current().signals.buckets[bucket].is_null() {
                let signal = current().signals.buckets[bucket];
                (*signal).close();
            }
        }
        while !current().observers.head.is_null() {
            unlink(current().observers.head);
        }
    }
}

/// Frame kind of runtime combinators; the P3 dispatcher passes such frames to [perry_combinator_step].
pub const COMBINATOR_KIND: u32 = u32::MAX;

const ALL: u32 = 0;
const RACE: u32 = 1;
const ALL_SETTLED: u32 = 2;

/// A `Promise.all`/`race`/`allSettled` in progress: a frame (kind first) that waits on every
/// pending input at once. Settlements update counters; one native event publishes the result.
#[repr(C)]
struct Combinator {
    kind: u32,
    mode: u32,
    result: *mut u8,
    inputs: *mut u8,
    remaining: u32,
    initialized: bool,
    completed: *mut Promise,
    signal: Signal,
    pin: u32,
    references: bool,
}

fn input(inputs: *mut u8, index: u32) -> *mut Promise {
    // SAFETY: an array of promises (layout of crate::array).
    unsafe {
        let data = *(inputs.add(crate::array::DATA as usize) as *const *const u64);
        *data.add(index as usize) as usize as *mut Promise
    }
}

/// Start a combinator over a snapshot of `inputs`, with observer storage for each pending one.
fn combine(mode: u32, inputs: *mut u8, collectible: bool, references: bool) -> *mut u8 {
    let result = if collectible {
        perry_promise_new_collectible(u32::from(mode != RACE || references))
    } else {
        perry_promise_new()
    };
    // SAFETY: compiled code passes an array of promises (layout of crate::array).
    let count = unsafe { *(inputs as *const u32) };
    let snapshot = if collectible {
        crate::array::perry_array_new_collectible(count, 1)
    } else {
        crate::array::perry_array_new(count)
    };
    let mut pending = 0;
    // SAFETY: the fresh snapshot has capacity for `count` slots.
    unsafe {
        let data = *snapshot.add(crate::array::DATA as usize).cast::<*mut u64>();
        for index in 0..count {
            let promise = input(inputs, index);
            *data.add(index as usize) = promise as usize as u64;
            pending += usize::from((*promise).state == PENDING);
        }
        *snapshot.cast::<u32>() = count;
    }
    let inputs = snapshot;
    let size = core::mem::size_of::<Combinator>() + pending * core::mem::size_of::<Observer>();
    let frame = if collectible {
        static SHAPE: [u32; 4] = [
            0,
            2,
            core::mem::offset_of!(Combinator, result) as u32,
            core::mem::offset_of!(Combinator, inputs) as u32,
        ];
        // SAFETY: exact pointer map of the frame.
        unsafe { crate::roots::perry_gc_alloc(size as u32, SHAPE.as_ptr()) }
    } else {
        current().allocate(size)
    } as *mut Combinator;
    let pin = if collectible {
        crate::roots::perry_gc_pin(frame.cast())
    } else {
        0
    };
    // SAFETY: freshly allocated.
    unsafe {
        frame.write(Combinator {
            kind: COMBINATOR_KIND,
            mode,
            result,
            inputs,
            remaining: 0,
            initialized: false,
            completed: ptr::null_mut(),
            signal: Signal::new(frame.cast(), SignalKind::Combinator),
            pin,
            references,
        })
    };
    perry_combinator_step(frame as *mut u8);
    result
}

/// `Promise.all(inputs)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_all(inputs: *mut u8) -> *mut u8 {
    combine(ALL, inputs, false, false)
}

/// `Promise.race(inputs)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_race(inputs: *mut u8) -> *mut u8 {
    combine(RACE, inputs, false, false)
}

/// `Promise.allSettled(inputs)`: fulfils with the settled inputs themselves, which compiled
/// code reads as `{ status, value, reason }` (the promise layout above).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_all_settled(inputs: *mut u8) -> *mut u8 {
    combine(ALL_SETTLED, inputs, false, false)
}

/// Typed combinator; `references` describes fulfilled input payloads.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_combine_collectible(
    mode: u32,
    inputs: *mut u8,
    references: u32,
) -> *mut u8 {
    combine(mode, inputs, true, references != 0)
}

fn reject_with(result: *mut u8, input: *mut Promise) {
    // SAFETY: a rejected input keeps its exception object in its value slot.
    unsafe {
        current().exception = (*input).value as usize as *mut u8;
        perry_promise_reject(result, (*input).status);
    }
}

/// Record bookkeeping without recursively resuming guest code or parent combinators.
unsafe fn accept_completion(combinator: &mut Combinator, promise: *mut Promise) {
    unsafe {
        // Each registered input observer completes exactly once.
        assert!(
            combinator.remaining != 0,
            "a combinator input completed twice"
        );
        combinator.remaining -= 1;
        if (*combinator.result.cast::<Promise>()).state != PENDING {
            release_combinator(combinator);
            return;
        }
        note_completion(combinator, promise);
        if combinator_ready(combinator) && combinator.signal.state == SignalState::Dormant {
            combinator.signal.arm();
            combinator.signal.fire();
        }
    }
}

/// Remember the first input that decides the result: any for `race`, a rejection for `all`.
unsafe fn note_completion(combinator: &mut Combinator, promise: *mut Promise) {
    unsafe {
        if combinator.completed.is_null()
            && (combinator.mode == RACE || (combinator.mode == ALL && (*promise).state == REJECTED))
        {
            combinator.completed = promise;
        }
    }
}

/// The result is decided: an input completed it, or every input of `all`/`allSettled` settled.
fn combinator_ready(combinator: &Combinator) -> bool {
    !combinator.completed.is_null() || (combinator.mode != RACE && combinator.remaining == 0)
}

/// Unpin a collectible combinator once it settled and no input observer refers to it.
unsafe fn release_combinator(combinator: &mut Combinator) {
    unsafe {
        if combinator.remaining == 0
            && combinator.pin != 0
            && (*combinator.result.cast::<Promise>()).state != PENDING
        {
            crate::roots::perry_gc_unpin(combinator.pin);
            combinator.pin = 0;
        }
    }
}

/// Initialize a combinator or publish the result selected by its native completion event.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_combinator_step(frame: *mut u8) {
    // SAFETY: generated dispatch supplies a live combinator frame, rooted by its signal or pin.
    unsafe {
        let frame = frame.cast::<Combinator>();
        let combinator = &mut *frame;
        let count = *combinator.inputs.cast::<u32>();
        if !combinator.initialized {
            combinator.initialized = true;
            for index in 0..count {
                let promise = input(combinator.inputs, index);
                if (*promise).state == PENDING {
                    let observers = frame.add(1).cast::<Observer>();
                    perry_promise_wait(
                        promise.cast(),
                        frame.cast(),
                        observers.add(combinator.remaining as usize).cast(),
                    );
                    combinator.remaining += 1;
                } else {
                    note_completion(combinator, promise);
                }
            }
        }
        if !combinator_ready(combinator) {
            return;
        }
        if !combinator.completed.is_null() {
            let completed = combinator.completed;
            if (*completed).state == REJECTED {
                reject_with(combinator.result, completed);
            } else {
                perry_promise_resolve(combinator.result, (*completed).value);
            }
        } else {
            let values = if combinator.pin != 0 {
                crate::array::perry_array_new_collectible(
                    count,
                    u32::from(combinator.mode == ALL_SETTLED || combinator.references),
                )
            } else {
                crate::array::perry_array_new(count)
            };
            let data = *values.add(crate::array::DATA as usize).cast::<*mut u64>();
            for index in 0..count {
                let promise = input(combinator.inputs, index);
                *data.add(index as usize) = if combinator.mode == ALL_SETTLED {
                    promise as usize as u64
                } else {
                    (*promise).value
                };
            }
            *values.cast::<u32>() = count;
            perry_promise_resolve(combinator.result, values as usize as u64);
        }
        release_combinator(combinator);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RuntimeInstance, region};

    #[test]
    fn shared_completions_allocate_no_per_observer_storage() {
        RuntimeInstance::default().enter(|| {
            region::perry_task_enter();
            let promise = perry_promise_new();
            let count = 10_000;
            let stride = (16 + core::mem::size_of::<Observer>()).next_multiple_of(8);
            let frames = current().allocate(stride * count);
            let before = current().allocations;
            for index in 0..count {
                unsafe {
                    let frame = frames.add(index * stride);
                    frame.cast::<u32>().write(0);
                    perry_promise_wait(promise, frame, frame.add(16));
                }
            }
            let allocations = current().allocations - before;
            assert_eq!(
                allocations, 1,
                "one shared native completion, no per-observer allocations"
            );
            perry_promise_resolve(promise, 7);
            perry_promise_resolve(promise, 8);
            let events = host::mock::completed_reactions();
            assert_eq!(events.len(), 1);
            let handle = events[0];
            let completion = perry_promise_event(host::EVENT_FUTURE_READ, handle);
            assert!(!completion.is_null());
            assert!(perry_promise_event(host::EVENT_FUTURE_READ, handle).is_null());
            for index in 0..count {
                assert_eq!(perry_promise_next(completion), unsafe {
                    frames.add(index * stride)
                });
            }
            assert_ne!(
                perry_promise_reuse(promise),
                promise,
                "active fan-out retains its promise"
            );
            assert!(perry_promise_next(completion).is_null());
            assert!(perry_promise_event(host::EVENT_FUTURE_READ, handle).is_null());
            assert!(current().signals.is_empty());
            assert!(current().observers.is_empty());
            let pending = perry_promise_new();
            perry_promise_wait(pending, frames, unsafe { frames.add(16) });
            region::perry_task_cancel();
            assert!(current().observers.is_empty());
            host::mock::with(|host| {
                assert!(
                    host.reactions
                        .iter()
                        .all(|reaction| !reaction.readable && !reaction.writable)
                )
            });
            region::perry_task_free();
        });
    }
}
