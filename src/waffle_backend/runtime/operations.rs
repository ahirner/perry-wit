//! Registered canonical operations retain storage until terminal acknowledgement.
use super::builder::{self, Builder};
use crate::waffle_backend::allocation::AllocationFuncs;
use anyhow::{Result, ensure};
use waffle::{Func, Memory, Module, Operator as O, Type::I32, Value};

const SCOPE: u32 = 112;
const SCOPE_FRAME: u32 = 116;
const NEXT: u32 = 0;
const PREVIOUS: u32 = 4;
const HANDLE: u32 = 8;
const THREAD: u32 = 12;
const STATE: u32 = 16;
const STATUS: u32 = 20;
const FRAME: u32 = 24;
const OWNER_SCOPE: u32 = 28;
const CONTROL: u32 = 32;
const EVENT: u32 = 36;
const SIZE: u32 = 40;
const PENDING: u32 = 0;
const CANCELLING: u32 = 1;
const TERMINAL: u32 = 2;

pub(crate) struct Transfer {
    pub(crate) event: u32,
    pub(crate) cancel: Func,
}

pub(crate) struct Imports {
    block_calls: Func,
    unblock_calls: Func,
    new_set: Func,
    drop_set: Func,
    join: Func,
    cancel: Func,
    drop: Func,
    index: Func,
    suspend: Func,
    resume: Func,
}
pub(crate) fn declare(module: &mut Module<'static>) -> Imports {
    Imports {
        block_calls: builder::native(module, "[backpressure-inc]", &[], &[]),
        unblock_calls: builder::native(module, "[backpressure-dec]", &[], &[]),
        new_set: builder::native(module, "[waitable-set-new]", &[], &[I32]),
        drop_set: builder::native(module, "[waitable-set-drop]", &[I32], &[]),
        join: builder::native(module, "[waitable-join]", &[I32, I32], &[]),
        cancel: builder::native(module, "[async-lower][subtask-cancel]", &[I32], &[I32]),
        drop: builder::native(module, "[subtask-drop]", &[I32], &[]),
        index: builder::native(module, "[thread-index]", &[], &[I32]),
        suspend: builder::native(module, "[thread-suspend]", &[], &[I32]),
        resume: builder::native(module, "[thread-resume-later]", &[I32], &[]),
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Operations {
    pub(crate) cancelled: Func,
    pub(crate) pending: Func,
    pub(crate) enter: Func,
    pub(crate) register: Func,
    pub(crate) register_transfer: Option<Func>,
    pub(crate) wait: Func,
    pub(crate) notify: Func,
    pub(crate) cancel: Func,
    pub(crate) cancel_all: Func,
    pub(crate) action: Func,
    pub(crate) idle: Func,
    pub(crate) finish: Func,
}
fn current_scope(b: &mut Builder) -> Value {
    let address = b.integer(SCOPE);
    b.load(address, 0, I32)
}
pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    a: AllocationFuncs,
    i: Imports,
    transfers: &[Transfer],
) -> Result<Operations> {
    ensure!(
        transfers
            .iter()
            .all(|transfer| (2..=5).contains(&transfer.event)),
        "Transfer controllers require stream/future read/write events"
    );
    let r = Operations {
        cancelled: builder::declare(module, "operations.cancelled", &[], &[I32]),
        pending: builder::declare(module, "operations.pending", &[], &[I32]),
        enter: builder::declare(module, "operations.enter", &[], &[]),
        register: builder::declare(module, "operations.register", &[I32], &[I32]),
        register_transfer: (!transfers.is_empty())
            .then(|| builder::declare(module, "operations.register-transfer", &[I32; 3], &[I32])),
        wait: builder::declare(module, "operations.wait", &[I32], &[I32]),
        notify: builder::declare(module, "operations.notify", &[I32; 3], &[]),
        cancel: builder::declare(module, "operations.cancel", &[I32], &[]),
        cancel_all: builder::declare(module, "operations.cancel-all", &[], &[]),
        action: builder::declare(module, "operations.action", &[], &[I32]),
        idle: builder::declare(module, "operations.idle", &[], &[I32]),
        finish: builder::declare(module, "operations.finish", &[], &[I32]),
    };
    for (function, offset) in [(r.cancelled, 8), (r.pending, 12)] {
        let mut b = Builder::new(module, function, memory);
        let scope = current_scope(&mut b);
        let present = b.body.add_block();
        let absent = b.body.add_block();
        b.branch(scope, present, absent);
        b.block = absent;
        let zero = b.integer(0);
        b.ret(&[zero]);
        b.block = present;
        let value = b.load(scope, offset, I32);
        b.ret(&[value]);
        b.finish(module, function)?;
    }
    let settle = builder::declare(module, "operations.settle", &[I32; 2], &[]);
    let mut b = Builder::new(module, r.enter, memory);
    let address = b.integer(SCOPE);
    let old = b.load(address, 0, I32);
    let clear = b.op(O::I32Eqz, &[old], I32);
    b.require(clear);
    b.call(i.block_calls, &[], &[]);
    let one = b.integer(1);
    let frame = b.call(a.frame_new, &[one], &[I32])[0];
    let root = b.integer(SCOPE_FRAME);
    b.store(root, 0, frame, I32);
    let scope = b.allocate(a.realloc, 16, 4);
    b.store(frame, 12, scope, I32);
    b.store(address, 0, scope, I32);
    let zero = b.integer(0);
    let size = b.integer(16);
    b.effect(O::MemoryFill { mem: memory }, &[scope, zero, size]);
    let set = b.call(i.new_set, &[], &[I32])[0];
    b.store(scope, 4, set, I32);
    b.ret(&[]);
    b.finish(module, r.enter)?;

    let register = builder::declare(module, "operations.retain", &[I32; 5], &[I32]);
    let mut b = Builder::new(module, r.register, memory);
    let packed = b.param(0);
    let mask = b.integer(15);
    let status = b.op(O::I32And, &[packed, mask], I32);
    let two = b.integer(2);
    let valid = b.op(O::I32LeU, &[status, two], I32);
    b.require(valid);
    let pending = b.op(O::I32LtU, &[status, two], I32);
    let four = b.integer(4);
    let handle = b.op(O::I32ShrU, &[packed, four], I32);
    let zero = b.integer(0);
    let event = b.integer(1);
    let owner = b.call(register, &[handle, status, zero, event, pending], &[I32])[0];
    b.ret(&[owner]);
    b.finish(module, r.register)?;

    if let Some(function) = r.register_transfer {
        let mut b = Builder::new(module, function, memory);
        let handle = b.param(0);
        let status = b.param(1);
        let control = b.param(2);
        let known = b.body.add_block();
        let event = b.body.add_blockparam(known, I32);
        for (index, transfer) in transfers.iter().enumerate() {
            let code = b.integer(index as u32 + 1);
            let same = b.op(O::I32Eq, &[control, code], I32);
            let selected = b.body.add_block();
            let following = b.body.add_block();
            b.branch(same, selected, following);
            b.block = selected;
            let event = b.integer(transfer.event);
            b.jump(known, &[event]);
            b.block = following;
        }
        b.body
            .set_terminator(b.block, waffle::Terminator::Unreachable);
        b.block = known;
        let blocked = b.integer(u32::MAX);
        let pending = b.op(O::I32Eq, &[status, blocked], I32);
        let owner = b.call(register, &[handle, status, control, event, pending], &[I32])[0];
        b.ret(&[owner]);
        b.finish(module, function)?;
    }

    let mut b = Builder::new(module, register, memory);
    let scope = current_scope(&mut b);
    b.require(scope);
    let handle = b.param(0);
    let status = b.param(1);
    let terminal = b.integer(TERMINAL);
    let one = b.integer(1);
    let frame = b.call(a.frame_new, &[one], &[I32])[0];
    let node = b.allocate(a.realloc, SIZE, 4);
    b.store(frame, 12, node, I32);
    let zero = b.integer(0);
    let size = b.integer(SIZE);
    b.effect(O::MemoryFill { mem: memory }, &[node, zero, size]);
    b.store(node, FRAME, frame, I32);
    b.store(node, OWNER_SCOPE, scope, I32);
    let none = b.integer(u32::MAX);
    b.store(node, THREAD, none, I32);
    b.store(node, CONTROL, b.param(2), I32);
    b.store(node, EVENT, b.param(3), I32);
    let returned = b.op(O::I32Eqz, &[b.param(4)], I32);
    let immediate = b.body.add_block();
    let pending = b.body.add_block();
    b.branch(returned, immediate, pending);
    b.block = immediate;
    b.store(node, STATE, terminal, I32);
    b.store(node, STATUS, status, I32);
    b.ret(&[node]);
    b.block = pending;
    b.store(node, HANDLE, handle, I32);
    let head = b.load(scope, 0, I32);
    b.store(node, NEXT, head, I32);
    b.store(scope, 0, node, I32);
    let nonempty = b.body.add_block();
    let join = b.body.add_block();
    b.branch(head, nonempty, join);
    b.block = nonempty;
    b.store(head, PREVIOUS, node, I32);
    b.jump(join, &[]);
    b.block = join;
    let count = b.load(scope, 12, I32);
    let count = b.op(O::I32Add, &[count, one], I32);
    b.store(scope, 12, count, I32);
    let set = b.load(scope, 4, I32);
    b.call(i.join, &[handle, set], &[]);
    let cancelled = b.load(scope, 8, I32);
    let cancel = b.body.add_block();
    let done = b.body.add_block();
    b.branch(cancelled, cancel, done);
    b.block = cancel;
    b.call(r.cancel, &[node], &[]);
    b.jump(done, &[]);
    b.block = done;
    b.ret(&[node]);
    b.finish(module, register)?;

    let mut b = Builder::new(module, r.wait, memory);
    let node = b.param(0);
    let state = b.load(node, STATE, I32);
    let terminal = b.integer(TERMINAL);
    let ready = b.op(O::I32Eq, &[state, terminal], I32);
    let finish = b.body.add_block();
    let wait = b.body.add_block();
    b.branch(ready, finish, wait);
    b.block = wait;
    let thread = b.call(i.index, &[], &[I32])[0];
    b.store(node, THREAD, thread, I32);
    b.call(i.suspend, &[], &[I32]);
    let state = b.load(node, STATE, I32);
    let ready = b.op(O::I32Eq, &[state, terminal], I32);
    b.require(ready);
    b.jump(finish, &[]);
    b.block = finish;
    let status = b.load(node, STATUS, I32);
    let frame = b.load(node, FRAME, I32);
    b.call(a.frame_drop, &[frame], &[]);
    let zero = b.integer(0);
    let size = b.integer(SIZE);
    let four = b.integer(4);
    b.call(a.realloc, &[node, size, four, zero], &[I32]);
    b.ret(&[status]);
    b.finish(module, r.wait)?;

    let mut b = Builder::new(module, settle, memory);
    let node = b.param(0);
    let status = b.param(1);
    let terminal = b.integer(TERMINAL);
    let state = b.load(node, STATE, I32);
    let pending = b.op(O::I32LtU, &[state, terminal], I32);
    b.require(pending);
    let handle = b.load(node, HANDLE, I32);
    let zero = b.integer(0);
    b.call(i.join, &[handle, zero], &[]);
    let event = b.load(node, EVENT, I32);
    let one = b.integer(1);
    let subtask = b.op(O::I32Eq, &[event, one], I32);
    let task = b.body.add_block();
    let transfer = b.body.add_block();
    let release = b.body.add_block();
    b.branch(subtask, task, transfer);
    b.block = task;
    let min = b.integer(2);
    let max = b.integer(4);
    let valid = b.op(O::I32GeU, &[status, min], I32);
    b.require(valid);
    let valid = b.op(O::I32LeU, &[status, max], I32);
    b.require(valid);
    b.call(i.drop, &[handle], &[]);
    b.jump(release, &[]);
    b.block = transfer;
    let mask = b.integer(15);
    let flag = b.op(O::I32And, &[status, mask], I32);
    let two = b.integer(2);
    let valid = b.op(O::I32LeU, &[flag, two], I32);
    b.require(valid);
    b.jump(release, &[]);
    b.block = release;
    b.store(node, STATE, terminal, I32);
    b.store(node, STATUS, status, I32);
    let scope = b.load(node, OWNER_SCOPE, I32);
    let previous = b.load(node, PREVIOUS, I32);
    let next = b.load(node, NEXT, I32);
    let before = b.op(O::Select, &[previous, scope, previous], I32);
    b.store(before, NEXT, next, I32);
    let has_next = b.body.add_block();
    let release = b.body.add_block();
    b.branch(next, has_next, release);
    b.block = has_next;
    b.store(next, PREVIOUS, previous, I32);
    b.jump(release, &[]);
    b.block = release;
    let count = b.load(scope, 12, I32);
    b.require(count);
    let one = b.integer(1);
    let count = b.op(O::I32Sub, &[count, one], I32);
    b.store(scope, 12, count, I32);
    let thread = b.load(node, THREAD, I32);
    let none = b.integer(u32::MAX);
    let waiting = b.op(O::I32Ne, &[thread, none], I32);
    let wake = b.body.add_block();
    let done = b.body.add_block();
    b.branch(waiting, wake, done);
    b.block = wake;
    b.call(i.resume, &[thread], &[]);
    b.jump(done, &[]);
    b.block = done;
    b.ret(&[]);
    b.finish(module, settle)?;

    let mut b = Builder::new(module, r.notify, memory);
    let one = b.integer(1);
    let subtask = b.op(O::I32Eq, &[b.param(0), one], I32);
    let two = b.integer(2);
    let started = b.op(O::I32LtU, &[b.param(2), two], I32);
    let started = b.op(O::I32And, &[subtask, started], I32);
    let terminal = b.op(O::I32Eqz, &[started], I32);
    let find = b.body.add_block();
    let done = b.body.add_block();
    b.branch(terminal, find, done);
    b.block = find;
    let scope = current_scope(&mut b);
    let head = b.load(scope, 0, I32);
    let search = b.body.add_block();
    let node = b.body.add_blockparam(search, I32);
    b.jump(search, &[head]);
    b.block = search;
    b.require(node);
    let handle = b.load(node, HANDLE, I32);
    let same = b.op(O::I32Eq, &[handle, b.param(1)], I32);
    let next = b.body.add_block();
    let found = b.body.add_block();
    b.branch(same, found, next);
    b.block = next;
    let next = b.load(node, NEXT, I32);
    b.jump(search, &[next]);
    b.block = found;
    let event = b.load(node, EVENT, I32);
    let valid = b.op(O::I32Eq, &[event, b.param(0)], I32);
    b.require(valid);
    b.call(settle, &[node, b.param(2)], &[]);
    b.jump(done, &[]);
    b.block = done;
    b.ret(&[]);
    b.finish(module, r.notify)?;

    let mut b = Builder::new(module, r.cancel, memory);
    let node = b.param(0);
    let state = b.load(node, STATE, I32);
    let pending = b.integer(PENDING);
    let pending = b.op(O::I32Eq, &[state, pending], I32);
    let cancel = b.body.add_block();
    let done = b.body.add_block();
    b.branch(pending, cancel, done);
    b.block = cancel;
    let cancelling = b.integer(CANCELLING);
    b.store(node, STATE, cancelling, I32);
    let zero = b.integer(0);
    let handle = b.load(node, HANDLE, I32);
    b.call(i.join, &[handle, zero], &[]);
    let control = b.load(node, CONTROL, I32);
    let acknowledged = b.body.add_block();
    let status = b.body.add_blockparam(acknowledged, I32);
    for (index, cancel) in std::iter::once(i.cancel)
        .chain(transfers.iter().map(|transfer| transfer.cancel))
        .enumerate()
    {
        let code = b.integer(index as u32);
        let same = b.op(O::I32Eq, &[control, code], I32);
        let selected = b.body.add_block();
        let following = b.body.add_block();
        b.branch(same, selected, following);
        b.block = selected;
        let status = b.call(cancel, &[handle], &[I32])[0];
        b.jump(acknowledged, &[status]);
        b.block = following;
    }
    b.body
        .set_terminator(b.block, waffle::Terminator::Unreachable);
    b.block = acknowledged;
    let blocked = b.integer(u32::MAX);
    let pending = b.op(O::I32Eq, &[status, blocked], I32);
    let wait = b.body.add_block();
    let complete = b.body.add_block();
    b.branch(pending, wait, complete);
    b.block = wait;
    let scope = b.load(node, OWNER_SCOPE, I32);
    let set = b.load(scope, 4, I32);
    b.call(i.join, &[handle, set], &[]);
    b.jump(done, &[]);
    b.block = complete;
    b.call(settle, &[node, status], &[]);
    b.jump(done, &[]);
    b.block = done;
    b.ret(&[]);
    b.finish(module, r.cancel)?;

    let mut b = Builder::new(module, r.cancel_all, memory);
    let scope = current_scope(&mut b);
    let one = b.integer(1);
    b.store(scope, 8, one, I32);
    let count = b.integer(2);
    let frame = b.call(a.frame_new, &[count], &[I32])[0];
    let head = b.load(scope, 0, I32);
    let next = b.body.add_block();
    let node = b.body.add_blockparam(next, I32);
    let visit = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[head]);
    b.block = next;
    b.branch(node, visit, done);
    b.block = visit;
    b.store(frame, 12, node, I32);
    let successor = b.load(node, NEXT, I32);
    b.store(frame, 16, successor, I32);
    b.call(r.cancel, &[node], &[]);
    b.jump(next, &[successor]);
    b.block = done;
    b.call(a.frame_drop, &[frame], &[]);
    b.ret(&[]);
    b.finish(module, r.cancel_all)?;

    let mut b = Builder::new(module, r.action, memory);
    let scope = current_scope(&mut b);
    let count = b.load(scope, 12, I32);
    let set = b.load(scope, 4, I32);
    let four = b.integer(4);
    let wait = b.op(O::I32Shl, &[set, four], I32);
    let two = b.integer(2);
    let wait = b.op(O::I32Or, &[wait, two], I32);
    let yield_ = b.integer(1);
    let action = b.op(O::Select, &[wait, yield_, count], I32);
    b.ret(&[action]);
    b.finish(module, r.action)?;

    let mut b = Builder::new(module, r.idle, memory);
    let scope = current_scope(&mut b);
    let set = b.load(scope, 4, I32);
    let four = b.integer(4);
    let wait = b.op(O::I32Shl, &[set, four], I32);
    let two = b.integer(2);
    let wait = b.op(O::I32Or, &[wait, two], I32);
    b.ret(&[wait]);
    b.finish(module, r.idle)?;

    let mut b = Builder::new(module, r.finish, memory);
    let scope = current_scope(&mut b);
    let count = b.load(scope, 12, I32);
    let clear = b.op(O::I32Eqz, &[count], I32);
    b.require(clear);
    let set = b.load(scope, 4, I32);
    b.call(i.drop_set, &[set], &[]);
    let cancelled = b.load(scope, 8, I32);
    let address = b.integer(SCOPE_FRAME);
    let frame = b.load(address, 0, I32);
    b.call(a.frame_drop, &[frame], &[]);
    let zero = b.integer(0);
    b.store(address, 0, zero, I32);
    let address = b.integer(SCOPE);
    b.store(address, 0, zero, I32);
    b.call(i.unblock_calls, &[], &[]);
    b.ret(&[cancelled]);
    b.finish(module, r.finish)?;
    Ok(r)
}

#[cfg(test)]
#[path = "operations_test.rs"]
mod tests;

#[cfg(test)]
#[path = "transfers_test.rs"]
mod transfers_test;
