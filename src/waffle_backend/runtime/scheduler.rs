//! Scheduling policy for source checkpoints, worker shutdown, and host callbacks.
//! Source continuations use P3's ready queue. Native owners park until acknowledged;
//! a source checkpoint polls the host only while competing work can advance.

use super::{
    builder::{self, Builder},
    operations::Operations,
};
use crate::waffle_backend::registry::ModuleRegistry;
use anyhow::Result;
use waffle::{Func, Module, Operator as O, Type::I32, Value};

const RUNNABLE_SOURCE: u32 = 88;
const LIVE_WORKERS: u32 = 120;
const NATIVE_WORKERS: u32 = 136;

#[derive(Clone, Copy)]
pub(crate) enum Worker {
    Source,
    Native,
}

#[derive(Clone, Copy)]
pub(crate) enum CountChange {
    Started,
    Finished,
}

pub(crate) enum Drain {
    Source,
    Workers,
}

pub(crate) enum HostTurn {
    Dispatch,
    SourceCallback { event: Value, source_done: Value },
}

fn count(b: &mut Builder, address: u32) -> Value {
    let address = b.integer(address);
    b.load(address, 0, I32)
}

fn update_count(b: &mut Builder, address: u32, change: CountChange) {
    let address = b.integer(address);
    let count = b.load(address, 0, I32);
    let one = b.integer(1);
    if matches!(change, CountChange::Finished) {
        b.require(count);
    }
    let operator = match change {
        CountChange::Started => O::I32Add,
        CountChange::Finished => O::I32Sub,
    };
    let count = b.op(operator, &[count, one], I32);
    b.store(address, 0, count, I32);
}

pub(crate) fn worker_count(b: &mut Builder, change: CountChange, kind: Worker) {
    update_count(b, LIVE_WORKERS, change);
    if matches!(kind, Worker::Native) {
        update_count(b, NATIVE_WORKERS, change);
    }
}

pub(crate) fn source_started(b: &mut Builder) {
    update_count(b, RUNNABLE_SOURCE, CountChange::Started);
}

pub(crate) fn enter_source(b: &mut Builder) {
    let address = b.integer(RUNNABLE_SOURCE);
    let one = b.integer(1);
    b.store(address, 0, one, I32);
    let address = b.integer(LIVE_WORKERS);
    let zero = b.integer(0);
    b.store(address, 0, zero, I32);
}

pub(crate) fn leave_source(b: &mut Builder) {
    let address = b.integer(RUNNABLE_SOURCE);
    let zero = b.integer(0);
    b.store(address, 0, zero, I32);
}

pub(crate) fn reset_workers(b: &mut Builder) {
    let zero = b.integer(0);
    for address in [LIVE_WORKERS, NATIVE_WORKERS] {
        let address = b.integer(address);
        b.store(address, 0, zero, I32);
    }
}

pub(crate) fn can_publish(b: &mut Builder, operations: Operations, source_done: Value) -> Value {
    let workers = count(b, LIVE_WORKERS);
    let pending = b.call(operations.pending, &[], &[I32])[0];
    let pending = b.op(O::I32Or, &[pending, workers], I32);
    let idle = b.op(O::I32Eqz, &[pending], I32);
    b.op(O::I32And, &[idle, source_done], I32)
}

pub(crate) fn host_action(b: &mut Builder, operations: Operations, turn: HostTurn) -> Value {
    let pending = b.call(operations.pending, &[], &[I32])[0];
    let wait = match turn {
        HostTurn::Dispatch => pending,
        HostTurn::SourceCallback { event, source_done } => {
            let source = count(b, RUNNABLE_SOURCE);
            let workers = count(b, NATIVE_WORKERS);
            let active = b.op(O::I32Or, &[source, workers], I32);
            let active = b.op(O::I32Or, &[active, event], I32);
            let active = b.op(O::I32Or, &[active, source_done], I32);
            let idle = b.op(O::I32Eqz, &[active], I32);
            b.op(O::I32Or, &[pending, idle], I32)
        }
    };
    let waiting = b.call(operations.wait_action, &[], &[I32])[0];
    let yielding = b.integer(1);
    b.op(O::Select, &[waiting, yielding, wait], I32)
}

pub(crate) struct SourceScheduler {
    pub(crate) checkpoint: Func,
    pub(crate) park_source: Func,
    pub(crate) complete_source: Func,
    pub(crate) wake_source: Func,
    pub(crate) start_eagerly: Func,
    pub(crate) park_native: Func,
    pub(crate) wake_native: Func,
    yield_host: Func,
    park_parent: Func,
    yield_parent: Func,
    context: Func,
}

impl SourceScheduler {
    pub(crate) fn declare(module: &mut Module<'static>) -> Self {
        Self {
            start_eagerly: builder::native(module, "[thread-yield-then-resume]", &[I32], &[I32]),
            park_native: builder::native(module, "[thread-suspend]", &[], &[I32]),
            wake_native: builder::native(module, "[thread-resume-later]", &[I32], &[]),
            yield_host: builder::native(module, "[thread-yield]", &[], &[I32]),
            park_parent: builder::native(module, "[thread-suspend-then-promote]", &[I32], &[I32]),
            yield_parent: builder::native(module, "[thread-yield-then-promote]", &[I32], &[I32]),
            context: builder::native(module, "[context-get-0]", &[], &[I32]),
            checkpoint: builder::declare(module, "tasks.checkpoint", &[], &[]),
            park_source: builder::declare(module, "tasks.park-source", &[], &[]),
            complete_source: builder::declare(module, "tasks.complete", &[], &[]),
            wake_source: builder::declare(module, "tasks.wake", &[I32], &[]),
        }
    }

    pub(crate) fn drain(&self, b: &mut Builder, scope: Drain) {
        let check = b.body.add_block();
        let advance = b.body.add_block();
        let done = b.body.add_block();
        b.jump(check, &[]);
        b.block = check;
        let (pending, handoff) = match scope {
            Drain::Source => {
                let runnable = count(b, RUNNABLE_SOURCE);
                let one = b.integer(1);
                (b.op(O::I32GtU, &[runnable, one], I32), self.checkpoint)
            }
            Drain::Workers => (count(b, LIVE_WORKERS), self.yield_host),
        };
        b.branch(pending, advance, done);
        b.block = advance;
        match scope {
            Drain::Source => {
                b.call(handoff, &[], &[]);
            }
            Drain::Workers => {
                b.call(handoff, &[], &[I32]);
            }
        }
        b.jump(check, &[]);
        b.block = done;
    }
}

pub(crate) fn emit(module: &mut Module<'static>, registry: &ModuleRegistry) -> Result<()> {
    let scheduler = &registry.promises.as_ref().unwrap().native.scheduler;
    let memory = registry.memory;
    let mut b = Builder::new(module, scheduler.wake_source, memory);
    source_started(&mut b);
    b.call(scheduler.wake_native, &[b.param(0)], &[]);
    b.ret(&[]);
    b.finish(module, scheduler.wake_source)?;

    enum Continuation {
        Park,
        Complete,
        Checkpoint,
    }
    for (function, continuation) in [
        (scheduler.park_source, Continuation::Park),
        (scheduler.complete_source, Continuation::Complete),
        (scheduler.checkpoint, Continuation::Checkpoint),
    ] {
        let mut b = Builder::new(module, function, memory);
        if !matches!(continuation, Continuation::Checkpoint) {
            update_count(&mut b, RUNNABLE_SOURCE, CountChange::Finished);
        }
        let context = b.call(scheduler.context, &[], &[I32])[0];
        let inspect = b.body.add_block();
        let handoff = b.body.add_block();
        let release = b.body.add_block();
        b.branch(context, inspect, release);
        b.block = inspect;
        let eager = b.load(context, 4, I32);
        b.branch(eager, handoff, release);
        b.block = handoff;
        let zero = b.integer(0);
        b.store(context, 4, zero, I32);
        let one = b.integer(1);
        let parent = b.op(O::I32Sub, &[eager, one], I32);
        let promote = match continuation {
            Continuation::Park => scheduler.park_parent,
            Continuation::Complete | Continuation::Checkpoint => scheduler.yield_parent,
        };
        b.call(promote, &[parent], &[I32]);
        b.ret(&[]);
        b.block = release;
        match continuation {
            Continuation::Checkpoint => {
                let runnable = count(&mut b, RUNNABLE_SOURCE);
                let one = b.integer(1);
                let alone = b.op(O::I32Eq, &[runnable, one], I32);
                let workers = count(&mut b, NATIVE_WORKERS);
                let pending = if let Some(operations) = registry.operations {
                    let pending = b.call(operations.pending, &[], &[I32])[0];
                    b.op(O::I32Or, &[workers, pending], I32)
                } else {
                    workers
                };
                let idle = b.op(O::I32Eqz, &[pending], I32);
                let uncontended = b.op(O::I32And, &[alone, idle], I32);
                let ready = b.body.add_block();
                let yield_host = b.body.add_block();
                b.branch(uncontended, ready, yield_host);
                b.block = ready;
                b.ret(&[]);
                b.block = yield_host;
                b.call(scheduler.yield_host, &[], &[I32]);
            }
            Continuation::Park => {
                b.call(scheduler.park_native, &[], &[I32]);
            }
            Continuation::Complete => {}
        }
        b.ret(&[]);
        b.finish(module, function)?;
    }
    Ok(())
}
