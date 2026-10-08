//! Serialized Web writer operations retain byte views through native acknowledgement.
use super::web::{self, BODY_KIND, ERROR, Helpers, LAST_READ, STATE, reject, tagged_allocation};
use crate::waffle_backend::runtime::builder::Builder;
use anyhow::Result;
use waffle::{
    Memory, Module, Operator as O,
    Type::{F64, I32},
};

pub(super) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    r: &web::Runtime<'_>,
    h: Helpers,
) -> Result<()> {
    let mut b = Builder::new(module, h.writable, memory);
    let stream = tagged_allocation(&mut b, r.allocator, 36, 16);
    let channel = b.op(O::I32TruncF64U, &[b.param(0)], I32);
    b.store(stream, BODY_KIND, channel, I32);
    b.ret(&[stream]);
    b.finish(module, h.writable)?;
    for (function, write) in [(h.write, true), (h.close, false)] {
        let mut b = Builder::new(module, function, memory);
        let stream = b.load(b.param(0), 4, I32);
        let detached = b.op(O::I32Eqz, &[stream], I32);
        reject(&mut b, detached, 12);
        let state = b.load(stream, STATE, I32);
        reject(&mut b, state, 12);
        let (Some(promises), Some(output)) = (r.promises, r.output) else {
            b.body
                .set_terminator(b.block, waffle::Terminator::Unreachable);
            b.finish(module, function)?;
            continue;
        };
        let zero = b.integer(0);
        let one = b.integer(1);
        let two = b.integer(2);
        let three = b.integer(3);
        if !write {
            b.store(stream, STATE, one, I32);
        }
        let frame = b.call(r.allocator.frame_new, &[three], &[I32])[0];
        b.store(frame, 12, stream, I32);
        b.store(frame, 16, b.param(1), I32);
        let previous = b.load(stream, LAST_READ, I32);
        b.store(frame, 20, previous, I32);
        let completion = b.call(promises.new, &[three], &[I32])[0];
        b.store(stream, LAST_READ, completion, I32);
        let wait = b.body.add_block();
        let ready = b.body.add_block();
        b.branch(previous, wait, ready);
        b.block = wait;
        b.call(promises.await_native, &[previous], &[I32, F64]);
        b.jump(ready, &[]);
        b.block = ready;
        let state = b.load(stream, STATE, I32);
        let failed = b.op(O::I32Eq, &[state, two], I32);
        let failure = b.body.add_block();
        let execute = b.body.add_block();
        let finish = b.body.add_block();
        let tag = b.body.add_blockparam(finish, I32);
        let payload = b.body.add_blockparam(finish, F64);
        b.branch(failed, failure, execute);
        b.block = failure;
        let error = b.load(stream, ERROR, I32);
        let error = b.op(O::F64ConvertI32U, &[error], F64);
        b.jump(finish, &[one, error]);
        b.block = execute;
        if write {
            let channel = b.load(stream, BODY_KIND, I32);
            let stdout = b.body.add_block();
            let stderr = b.body.add_block();
            b.branch(channel, stderr, stdout);
            for (block, function) in [(stdout, output[0]), (stderr, output[1])] {
                b.block = block;
                if let Some(function) = function {
                    let result = b.call(function, &[b.param(1)], &[I32, F64]);
                    b.jump(finish, &result);
                } else {
                    b.body
                        .set_terminator(b.block, waffle::Terminator::Unreachable);
                }
            }
        } else {
            let empty = b.number(0.0);
            b.jump(finish, &[zero, empty]);
        }
        b.block = finish;
        let save_error = b.body.add_block();
        let settle = b.body.add_block();
        b.branch(tag, save_error, settle);
        b.block = save_error;
        b.store(stream, STATE, two, I32);
        let error = b.op(O::I32TruncF64U, &[payload], I32);
        b.store(stream, ERROR, error, I32);
        b.jump(settle, &[]);
        b.block = settle;
        b.call(promises.native.settle, &[completion, tag, payload], &[]);
        b.call(r.allocator.frame_drop, &[frame], &[]);
        b.ret(&[tag, payload]);
        b.finish(module, function)?;
    }
    Ok(())
}
