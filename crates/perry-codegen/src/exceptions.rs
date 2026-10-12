//! Exceptions under the status convention: throw targets, raising and catching, and the
//! exception object ops.
use crate::{
    function::FunctionLowering,
    repr::{Repr, i32_type, repr, val_type_is_i64},
    runtime::Runtime,
    status_code,
};
use anyhow::{Result, bail};
use perry_abi::layout::exception::{KIND, TAG_BIGINT, TAG_BOOLEAN, TAG_NUMBER, TAG_STRING};
use perry_mir::{
    attrs::ErrorKind,
    exceptions::{
        CatchOp, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExitOp, RaiseOp, RethrowOp,
    },
    types::{BigIntType, ExceptionType, StringType},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::types::FP64Type,
    context::{Context, Ptr},
    op::Op,
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::ReturnOp,
};

impl FunctionLowering<'_, '_> {
    /// Lower an exception op; `false` if `op` is none.
    pub(crate) fn exception_op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<bool> {
        if let Some(error) = Operation::get_op::<ErrorNewOp>(op, ctx) {
            let kind = self.i32_const(ctx, status_code(error.kind(ctx)));
            let tag = self.i32_const(ctx, TAG_STRING as i32);
            let message = self.operand(ctx, op, 0)?;
            let message = self.num(ctx, NumOpcode::I64ExtendI32U, vec![message]);
            let value = self.runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, message]);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<RaiseOp>(op, ctx).is_some() {
            let exception = self.thrown(ctx, op)?;
            let status = self.runtime(ctx, Runtime::ExceptionRaise, vec![exception]);
            self.define(ctx, op, status);
            return Ok(true);
        }
        if Operation::get_op::<ExitOp>(op, ctx).is_some() {
            let code = self.operand(ctx, op, 0)?;
            let status = self.runtime(ctx, Runtime::Exit, vec![code]);
            self.define(ctx, op, status);
            return Ok(true);
        }
        if Operation::get_op::<RethrowOp>(op, ctx).is_some() {
            let status = self.operand(ctx, op, 0)?;
            let throw = self.throw_block(ctx);
            self.br(ctx, throw, vec![status]);
            return Ok(true);
        }
        if Operation::get_op::<CatchOp>(op, ctx).is_some() {
            let status = self.operand(ctx, op, 0)?;
            let value = self.runtime(ctx, Runtime::ExceptionCatch, vec![status]);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ExceptionMessageOp>(op, ctx).is_some() {
            let exception = self.operand(ctx, op, 0)?;
            let value = self.runtime(ctx, Runtime::ExceptionMessage, vec![exception]);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if let Some(is) = Operation::get_op::<ExceptionIsOp>(op, ctx) {
            let exception = self.operand(ctx, op, 0)?;
            let kind = self.load(ctx, MemKind::I32, exception, KIND);
            let value = match is.kind(ctx) {
                ErrorKind::Any => {
                    // TypeError, RangeError and Error are 1..=3.
                    let one = self.i32_const(ctx, 1);
                    let offset = self.num(ctx, NumOpcode::I32Sub, vec![kind, one]);
                    let three = self.i32_const(ctx, 3);
                    self.num(ctx, NumOpcode::I32LtU, vec![offset, three])
                }
                other => {
                    let code = self.i32_const(ctx, status_code(other));
                    self.num(ctx, NumOpcode::I32Eq, vec![kind, code])
                }
            };
            self.define(ctx, op, value);
            return Ok(true);
        }
        Ok(false)
    }

    /// The exception object `op`'s operand throws: itself, or a thrown value boxed.
    pub(crate) fn thrown(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<Value> {
        let value = self.operand(ctx, op, 0)?;
        let ty = op.deref(ctx).get_operand(0).get_type(ctx);
        if ty.deref(ctx).is::<ExceptionType>() {
            return Ok(value);
        }
        let tag = match repr(ctx, ty)? {
            Repr::F64 => TAG_NUMBER,
            Repr::Bool => TAG_BOOLEAN,
            _ if ty.deref(ctx).is::<StringType>() => TAG_STRING,
            _ if ty.deref(ctx).is::<BigIntType>() => TAG_BIGINT,
            _ => bail!("this value cannot be thrown yet"),
        };
        let kind = self.i32_const(ctx, status_code(ErrorKind::Value));
        let tag = self.i32_const(ctx, tag as i32);
        let bits = self.slot_bits(ctx, value);
        Ok(self.runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, bits]))
    }

    /// The block that returns zero results with the status in its argument.
    pub(crate) fn throw_block(&mut self, ctx: &mut Context) -> Ptr<BasicBlock> {
        if let Some(block) = self.throw_block {
            return block;
        }
        if self.frame.is_some() {
            let block = self.reject_block(ctx);
            self.throw_block = Some(block);
            return block;
        }
        let i32 = i32_type(ctx);
        let block = self.new_block(ctx, vec![i32]);
        let saved = std::mem::replace(&mut self.block, block);
        let mut values = Vec::with_capacity(self.results.len() + 1);
        for index in 0..self.results.len() {
            let zero = self.zero(ctx, self.results[index]);
            values.push(zero);
        }
        values.push(block.deref(ctx).get_argument(0));
        self.leave_roots(ctx);
        let op = ReturnOp::new(ctx, values).get_operation();
        self.insert(ctx, op);
        self.block = saved;
        self.throw_block = Some(block);
        block
    }

    /// Where an exception goes: the landing of an op followed by `perry.caught`, or out of the
    /// function.
    pub(crate) fn throw_target(
        &mut self,
        ctx: &mut Context,
        status: Value,
    ) -> (Ptr<BasicBlock>, Vec<Value>) {
        // The landing stays in place for later throws of the same op.
        let Some((landing, types)) = self.landing.take() else {
            return (self.throw_block(ctx), vec![status]);
        };
        let mut args = vec![status];
        for &ty in &types {
            args.push(self.zero(ctx, ty));
        }
        self.landing = Some((landing, types));
        (landing, args)
    }

    /// Throw `status` if `failed` is non-zero; code after this runs on the normal path.
    pub(crate) fn throw_if(&mut self, ctx: &mut Context, failed: Value, kind: ErrorKind) {
        let status = self.i32_const(ctx, status_code(kind));
        self.throw_status_if(ctx, failed, status);
    }

    /// Throw with `status` if `failed` is non-zero.
    pub(crate) fn throw_status_if(&mut self, ctx: &mut Context, failed: Value, status: Value) {
        debug_assert!(
            self.throws,
            "throwing op in a function classified as non-throwing"
        );
        let throw = self.throw_target(ctx, status);
        let next = self.new_block(ctx, vec![]);
        self.cond_br(ctx, failed, throw, (next, vec![]));
        self.block = next;
    }

    /// The zero of Wasm type `ty`.
    fn zero(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        if ty.deref(ctx).is::<FP64Type>() {
            self.f64_const(ctx, 0.0)
        } else if val_type_is_i64(ctx, ty) {
            self.i64_const(ctx, 0)
        } else {
            self.i32_const(ctx, 0)
        }
    }
}
