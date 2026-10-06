//! WIT byte-stream ownership within an invocation, using the Web reader queue.
use super::web::{BODY_KIND, CANCELLING, EOF, Helpers, LOCK, STATE, TRANSFER, tagged_allocation};
use crate::waffle_backend::runtime::{
    builder::{self, Builder},
    operations::Operations,
};
use crate::waffle_backend::runtime::{
    imports,
    operations::{Cancellation, Transfer},
};
use anyhow::Result;
use waffle::{
    Func, Memory, Module, Operator as O,
    Type::{F64, I32},
};

pub(crate) const HANDLE: u32 = 36;
pub(crate) const USED: u32 = 40;
const ROOT: u32 = 44;
pub(crate) const KIND: u32 = 2;
pub(crate) const SIZE: u32 = 48;

#[derive(Clone, Copy)]
pub(crate) struct Imports {
    pub read: Func,
    pub drop: Func,
    pub cancel: Option<Func>,
}
impl Imports {
    pub(crate) fn declare(module: &mut Module<'static>, asynchronous: bool) -> Self {
        let mut definitions = vec![
            imports::Function {
                name: if asynchronous { "async-read" } else { "read" }.into(),
                params: vec!["i32"; 3],
                results: vec!["i32"],
            },
            imports::Function {
                name: "drop".into(),
                params: vec!["i32"],
                results: vec![],
            },
        ];
        if asynchronous {
            definitions.push(imports::Function {
                name: "cancel-read".into(),
                params: vec!["i32"],
                results: vec!["i32"],
            });
        }
        let imports = imports::declare_imports(module, "streams", &definitions);
        Self {
            read: imports[if asynchronous { "async-read" } else { "read" }],
            drop: imports["drop"],
            cancel: imports.get("cancel-read").copied(),
        }
    }
    pub(crate) fn controller(self) -> Option<Transfer> {
        self.cancel.map(|cancel| Transfer {
            event: 2,
            cancellation: Cancellation::Cancel(cancel),
        })
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Runtime {
    pub imports: Imports,
    pub operations: Option<Operations>,
    pub controller: u32,
}

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    r: &super::web::Runtime<'_>,
    h: Helpers,
    finish: Func,
) -> Result<Func> {
    let mut b = Builder::new(module, h.input, memory);
    let one = b.integer(1);
    let frame = b.call(r.allocator.frame_new, &[one], &[I32])[0];
    let stream = tagged_allocation(&mut b, r.allocator, SIZE, 16);
    b.store(frame, 12, stream, I32);
    b.store(stream, HANDLE, b.param(0), I32);
    b.store(stream, ROOT, frame, I32);
    let kind = b.integer(KIND);
    b.store(stream, BODY_KIND, kind, I32);
    b.ret(&[stream]);
    b.finish(module, h.input)?;

    let mut b = Builder::new(module, h.dispose, memory);
    let stream = b.param(0);
    let lock = b.load(stream, LOCK, I32);
    let receiver = b.op(O::Select, &[lock, stream, lock], I32);
    let zero = b.integer(0);
    b.call(h.cancel, &[receiver, lock, zero], &[I32, F64]);
    let frame = b.load(stream, ROOT, I32);
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[]);
    b.finish(module, h.dispose)?;

    let pull = builder::declare(module, "web.pull-input", &[I32], &[I32; 3]);
    let mut b = Builder::new(module, pull, memory);
    let stream = b.param(0);
    let Some(native) = r.input else {
        b.body
            .set_terminator(b.block, waffle::Terminator::Unreachable);
        b.finish(module, pull)?;
        return Ok(pull);
    };
    let zero = b.integer(0);
    let one = b.integer(1);
    let two = b.integer(2);
    let frame = b.call(r.allocator.frame_new, &[two], &[I32])[0];
    b.store(frame, 12, stream, I32);
    let data = b.allocate(r.allocator.realloc, 8192, 1);
    b.store(frame, 16, data, I32);
    let handle = b.load(stream, HANDLE, I32);
    let capacity = b.integer(8192);
    let transfer = b.body.add_block();
    b.jump(transfer, &[]);
    b.block = transfer;
    let status = b.call(native.imports.read, &[handle, data, capacity], &[I32])[0];
    let packed = if let Some(operations) = native.operations {
        let controller = b.integer(native.controller);
        let owner = b.call(
            operations.register_transfer.unwrap(),
            &[handle, status, controller],
            &[I32],
        )[0];
        b.store(stream, TRANSFER, owner, I32);
        let status = b.call(operations.wait, &[owner], &[I32])[0];
        b.store(stream, TRANSFER, zero, I32);
        status
    } else {
        status
    };
    let mask = b.integer(15);
    let shift = b.integer(4);
    let status = b.op(O::I32And, &[packed, mask], I32);
    let length = b.op(O::I32ShrU, &[packed, shift], I32);
    let valid = b.op(O::I32LeU, &[status, two], I32);
    b.require(valid);
    let valid = b.op(O::I32LeU, &[length, capacity], I32);
    b.require(valid);
    let state = b.load(stream, STATE, I32);
    let cancelling = b.integer(CANCELLING);
    let stopped = b.op(O::I32Eq, &[state, cancelling], I32);
    let interrupted = b.op(O::I32Eq, &[status, two], I32);
    let stopped = b.op(O::I32Or, &[stopped, interrupted], I32);
    let cancel = b.body.add_block();
    let received = b.body.add_block();
    b.branch(stopped, cancel, received);
    b.block = cancel;
    let reason = b.call(finish, &[stream], &[I32])[0];
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[reason, zero, one]);
    b.block = received;
    let closed = b.op(O::I32Eq, &[status, one], I32);
    b.store(stream, EOF, closed, I32);
    let bytes = b.body.add_block();
    let empty = b.body.add_block();
    b.branch(length, bytes, empty);
    b.block = empty;
    let end = b.body.add_block();
    b.branch(closed, end, transfer);
    b.block = end;
    let reason = b.call(finish, &[stream], &[I32])[0];
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[reason, zero, one]);
    b.block = bytes;
    let view = b.call(r.bytes.lift_canonical, &[data, length], &[I32])[0];
    b.call(r.allocator.frame_drop, &[frame], &[]);
    b.ret(&[zero, view, zero]);
    b.finish(module, pull)?;
    Ok(pull)
}
