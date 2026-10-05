//! Statically typed abort controllers and dependent signals on the guest heap.
use super::{
    allocation::AllocationFuncs,
    runtime::{
        builder::{self, Builder},
        operations::Operations,
    },
};
use anyhow::Result;
use perry_hir::types::Type;
use waffle::{Func, Memory, Module, Operator as O, Type::I32};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Controller,
    Signal,
}
impl Kind {
    pub(crate) fn of(ty: &Type) -> Option<Self> {
        [Self::Controller, Self::Signal]
            .into_iter()
            .find(|kind| *ty == Type::Named(kind.type_name().into()))
    }
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            Self::Controller => "__perry_abort_controller",
            Self::Signal => "__perry_abort_signal",
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Helpers {
    pub(crate) new: Func,
    pub(crate) follow: Func,
    pub(crate) root: Func,
    pub(crate) aborted: Func,
    pub(crate) abort: Func,
    pub(crate) notify: Func,
}
pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    a: AllocationFuncs,
    operations: Option<Operations>,
) -> Result<Helpers> {
    let r = Helpers {
        new: builder::declare(module, "abort.new", &[], &[I32]),
        follow: builder::declare(module, "abort.follow", &[I32], &[I32]),
        root: builder::declare(module, "abort.root", &[I32], &[I32]),
        aborted: builder::declare(module, "abort.aborted", &[I32], &[I32]),
        abort: builder::declare(module, "abort.abort", &[I32], &[]),
        notify: builder::declare(module, "abort.notify", &[I32], &[]),
    };
    let mut b = Builder::new(module, r.root, memory);
    let signal = b.param(0);
    let present = b.body.add_block();
    let absent = b.body.add_block();
    b.branch(signal, present, absent);
    b.block = absent;
    b.ret(&[signal]);
    b.block = present;
    let parent = b.load(signal, 4, I32);
    let root = b.op(O::Select, &[parent, signal, parent], I32);
    b.ret(&[root]);
    b.finish(module, r.root)?;

    let mut b = Builder::new(module, r.follow, memory);
    let parent = b.call(r.root, &[b.param(0)], &[I32])[0];
    let one = b.integer(1);
    let frame = b.call(a.frame_new, &[one], &[I32])[0];
    b.store(frame, 12, parent, I32);
    let signal = b.allocate(a.realloc, 8, 4);
    let four = b.integer(4);
    let header = b.op(O::I32Sub, &[signal, four], I32);
    let header = b.load(header, 0, I32);
    let kind = b.integer(5);
    b.store(header, 16, kind, I32);
    let zero = b.integer(0);
    b.store(signal, 0, zero, I32);
    b.store(signal, 4, parent, I32);
    b.call(a.frame_drop, &[frame], &[]);
    b.ret(&[signal]);
    b.finish(module, r.follow)?;

    let mut b = Builder::new(module, r.new, memory);
    let zero = b.integer(0);
    let signal = b.call(r.follow, &[zero], &[I32])[0];
    let controller = b.call(r.follow, &[signal], &[I32])[0];
    b.ret(&[controller]);
    b.finish(module, r.new)?;

    let mut b = Builder::new(module, r.aborted, memory);
    let signal = b.call(r.root, &[b.param(0)], &[I32])[0];
    let present = b.body.add_block();
    let absent = b.body.add_block();
    b.branch(signal, present, absent);
    b.block = absent;
    b.ret(&[signal]);
    b.block = present;
    let aborted = b.load(signal, 0, I32);
    b.ret(&[aborted]);
    b.finish(module, r.aborted)?;

    let mut b = Builder::new(module, r.abort, memory);
    let signal = b.load(b.param(0), 4, I32);
    if let Some(operations) = operations {
        b.call(operations.abort_signal, &[signal], &[]);
    } else {
        let one = b.integer(1);
        b.store(signal, 0, one, I32);
    }
    b.call(r.notify, &[signal], &[]);
    b.ret(&[]);
    b.finish(module, r.abort)?;
    let mut b = Builder::new(module, r.notify, memory);
    b.ret(&[]);
    b.finish(module, r.notify)?;
    Ok(r)
}
