//! Lower exception values and the local landing of a caught operation.
use super::*;
use perry_mir::exceptions::{
    BoxExceptionOp, CatchOp, ErrorNewOp, ExceptionIsOp, ExceptionMessageOp, RaiseOp, RethrowOp,
};
use perry_runtime::exception as tags;

impl FunctionLowering<'_> {
    pub(super) fn exception_target(
        &mut self,
        ctx: &mut Context,
        status: Value,
    ) -> (Ptr<BasicBlock>, Vec<Value>) {
        if let Some((block, types)) = self.landing.clone() {
            let mut args = vec![status];
            for ty in types {
                args.push(self.zero(ctx, ty));
            }
            (block, args)
        } else {
            (self.throw_block(ctx), vec![status])
        }
    }

    pub(super) fn lower_exception_op(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
    ) -> Result<bool> {
        if Operation::get_op::<RethrowOp>(op, ctx).is_some() {
            let status = self.operand(ctx, op, 0)?;
            let (block, args) = self.exception_target(ctx, status);
            self.br(ctx, block, args);
            return Ok(true);
        }
        let runtime = if Operation::get_op::<RaiseOp>(op, ctx).is_some() {
            Some(Runtime::ExceptionRaise)
        } else if Operation::get_op::<CatchOp>(op, ctx).is_some() {
            Some(Runtime::ExceptionCatch)
        } else if Operation::get_op::<ExceptionMessageOp>(op, ctx).is_some() {
            Some(Runtime::ExceptionMessage)
        } else {
            None
        };
        if let Some(runtime) = runtime {
            let args = self.operands(ctx, op)?;
            let value = self.runtime(ctx, runtime, args).expect("exception result");
            self.define(ctx, op, value);
            return Ok(true);
        }
        if let Some(test) = Operation::get_op::<ExceptionIsOp>(op, ctx) {
            let value = self.operand(ctx, op, 0)?;
            let kind = self.i32_const(ctx, status_code(test.kind(ctx)));
            let value = self
                .runtime(ctx, Runtime::ExceptionIs, vec![value, kind])
                .expect("boolean");
            self.define(ctx, op, value);
            return Ok(true);
        }
        let error = Operation::get_op::<ErrorNewOp>(op, ctx);
        let boxed = Operation::get_op::<BoxExceptionOp>(op, ctx).is_some();
        if error.is_some() || boxed {
            let value = self.operand(ctx, op, 0)?;
            let ty = op.deref(ctx).get_operand(0).get_type(ctx);
            if boxed && ty.deref(ctx).is::<ExceptionType>() {
                self.define(ctx, op, value);
                return Ok(true);
            }
            let tag = if let Some(error) = error {
                status_code(error.kind(ctx))
            } else if ty.deref(ctx).is::<FP64Type>() {
                tags::NUMBER as i32
            } else if ty.deref(ctx).is::<StringType>() {
                tags::STRING as i32
            } else {
                tags::BOOLEAN as i32
            };
            let tag = self.i32_const(ctx, tag);
            let bits = self.slot_bits(ctx, value);
            let value = self
                .runtime(ctx, Runtime::ExceptionNew, vec![tag, bits])
                .expect("exception");
            self.define(ctx, op, value);
            return Ok(true);
        }
        Ok(false)
    }
}
