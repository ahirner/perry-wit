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
    BigIntType, CatchOp, ErrorKind, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, ExceptionType,
    ExitOp, RaiseOp, RethrowOp, StringType,
};
use pliron::{
    basic_block::BasicBlock,
    builtin::types::FP64Type,
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_, '_> {
    /// Lower an exception operation, returning `false` for other operations.
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
                    let basic = self.num(ctx, NumOpcode::I32LtU, vec![offset, three]);
                    let six = self.i32_const(ctx, 6);
                    let offset = self.num(ctx, NumOpcode::I32Sub, vec![kind, six]);
                    let two = self.i32_const(ctx, 2);
                    let native = self.num(ctx, NumOpcode::I32LtU, vec![offset, two]);
                    self.num(ctx, NumOpcode::I32Or, vec![basic, native])
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
        if ty.deref(ctx).is::<perry_mir::JsonType>() {
            return Ok(self.runtime(ctx, Runtime::StdJsonToException, vec![value]));
        }
        let inner = crate::repr::optional_inner(ctx, ty).unwrap_or(ty);
        let tag = match repr(ctx, inner)? {
            Repr::F64 => TAG_NUMBER,
            Repr::Bool => TAG_BOOLEAN,
            _ if inner.deref(ctx).is::<StringType>() => TAG_STRING,
            _ if inner.deref(ctx).is::<BigIntType>() => TAG_BIGINT,
            _ => bail!("this value cannot be thrown yet"),
        };
        let kind = self.i32_const(ctx, status_code(ErrorKind::Value));
        let mut tag = self.i32_const(ctx, tag as i32);
        if inner != ty {
            let null = self.absence_test(ctx, value, ty, true)?;
            let undefined = self.absence_test(ctx, value, ty, false)?;
            let null_tag = self.i32_const(ctx, perry_abi::layout::exception::TAG_NULL as i32);
            let undefined_tag =
                self.i32_const(ctx, perry_abi::layout::exception::TAG_UNDEFINED as i32);
            tag = self.select(ctx, null, null_tag, tag);
            tag = self.select(ctx, undefined, undefined_tag, tag);
        }
        let bits = self.slot_bits(ctx, value);
        Ok(self.runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, bits]))
    }

    /// The block that returns zero results with the status in its argument.
    fn throw_block(&mut self, ctx: &mut Context) -> Ptr<BasicBlock> {
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
        self.in_block(block, |b| {
            let mut values = Vec::with_capacity(b.results.len() + 1);
            for index in 0..b.results.len() {
                let zero = b.zero(ctx, b.results[index]);
                values.push(zero);
            }
            values.push(block.deref(ctx).get_argument(0));
            b.leave_roots(ctx);
            b.ret(ctx, values);
        });
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
        debug_assert!(
            self.throws,
            "throwing op in a function classified as non-throwing"
        );
        // The landing stays in place for later throws of the same op.
        let Some((landing, types)) = self.landing.take() else {
            return (self.throw_block(ctx), vec![status]);
        };
        let mut args = Vec::with_capacity(types.len() + 1);
        args.push(status);
        for &ty in &types {
            args.push(self.zero(ctx, ty));
        }
        self.landing = Some((landing, types));
        (landing, args)
    }

    /// Branch to the exception handler with `kind` when `failed` is non-zero, continuing on the normal path otherwise.
    pub(crate) fn throw_if(&mut self, ctx: &mut Context, failed: Value, kind: ErrorKind) {
        let raise = self.new_block(ctx, vec![]);
        let next = self.new_block(ctx, vec![]);
        self.cond_br(ctx, failed, (raise, vec![]), (next, vec![]));
        self.in_block(raise, |b| {
            let status = b.i32_const(ctx, status_code(kind));
            let status = b.runtime(ctx, Runtime::ExceptionImplicit, vec![status]);
            let (target, args) = b.throw_target(ctx, status);
            b.br(ctx, target, args);
        });
        self.block = next;
    }

    /// Throw with `status` if `failed` is non-zero.
    pub(crate) fn throw_status_if(&mut self, ctx: &mut Context, failed: Value, status: Value) {
        let throw = self.throw_target(ctx, status);
        let next = self.new_block(ctx, vec![]);
        self.cond_br(ctx, failed, throw, (next, vec![]));
        self.block = next;
    }

    /// The zero of Wasm type `ty`.
    pub(crate) fn zero(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        if ty.deref(ctx).is::<FP64Type>() {
            self.f64_const(ctx, 0.0)
        } else if val_type_is_i64(ctx, ty) {
            self.i64_const(ctx, 0)
        } else {
            self.i32_const(ctx, 0)
        }
    }
}
