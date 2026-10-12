//! Optional values: the `undefined` and `null` sentinels of each representation, absence
//! tests and the optional ops.
use crate::{
    UNDEFINED_BOOL,
    function::FunctionLowering,
    repr::{Repr, repr},
};
use anyhow::{Result, anyhow, bail};
use perry_abi::UNDEFINED_BITS;
use perry_mir::{
    attrs::ErrorKind,
    optional::{
        IsNullOp, IsUndefinedOp, NullOp, OptionalCastOp, ToNumberOp, ToStringOp, UndefinedOp,
        UnwrapOp, WrapOp, absence,
    },
    views::AggregateViewOp,
};
use pliron::{
    builtin::op_interfaces::OneResultInterface,
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_wasm::attrs::NumOpcode;

impl FunctionLowering<'_, '_> {
    /// Lower an optional-value op; `false` if `op` is none.
    pub(crate) fn optional_op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<bool> {
        if let Some(null) = Operation::get_op::<NullOp>(op, ctx) {
            let value = self.null(ctx, null.get_result(ctx).get_type(ctx))?;
            self.define(ctx, op, value);
            return Ok(true);
        }
        if let Some(undefined) = Operation::get_op::<UndefinedOp>(op, ctx) {
            let value = self.undefined(ctx, undefined.get_result(ctx).get_type(ctx))?;
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<WrapOp>(op, ctx).is_some()
            || Operation::get_op::<AggregateViewOp>(op, ctx).is_some()
        {
            let value = self.operands(ctx, op)?[0];
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<OptionalCastOp>(op, ctx).is_some() {
            let source = op.deref(ctx).get_operand(0).get_type(ctx);
            let target = op.deref(ctx).get_result(0).get_type(ctx);
            let value = self.operand(ctx, op, 0)?;
            let value = self.optional_cast(ctx, value, source, target)?;
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<IsUndefinedOp>(op, ctx).is_some()
            || Operation::get_op::<IsNullOp>(op, ctx).is_some()
        {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped(ctx, mir)?;
            let null = Operation::get_op::<IsNullOp>(op, ctx).is_some();
            let result = self.absence_test(ctx, value, mir.get_type(ctx), null)?;
            self.define(ctx, op, result);
            return Ok(true);
        }
        if Operation::get_op::<ToNumberOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped(ctx, mir)?;
            let undefined = self.absence_test(ctx, value, mir.get_type(ctx), false)?;
            let null = self.absence_test(ctx, value, mir.get_type(ctx), true)?;
            let nan = self.f64_const(ctx, f64::NAN);
            let zero = self.f64_const(ctx, 0.0);
            let number = self.select(ctx, null, zero, value);
            let result = self.select(ctx, undefined, nan, number);
            self.define(ctx, op, result);
            return Ok(true);
        }
        if Operation::get_op::<UnwrapOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped(ctx, mir)?;
            let absent = self.is_absent(ctx, value, mir.get_type(ctx))?;
            self.throw_if(ctx, absent, ErrorKind::TypeError);
            self.define(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<ToStringOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped(ctx, mir)?;
            let undefined = self.absence_test(ctx, value, mir.get_type(ctx), false)?;
            let null = self.absence_test(ctx, value, mir.get_type(ctx), true)?;
            let undefined_text = self.state.data.string("undefined");
            let null_text = self.state.data.string("null");
            let undefined_text = self.i32_const(ctx, undefined_text as i32);
            let null_text = self.i32_const(ctx, null_text as i32);
            let text = self.select(ctx, null, null_text, value);
            let result = self.select(ctx, undefined, undefined_text, text);
            self.define(ctx, op, result);
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn undefined(&mut self, ctx: &mut Context, inner: TypeHandle) -> Result<Value> {
        Ok(match repr(ctx, inner)? {
            Repr::F64 => {
                let bits = self.i64_const(ctx, UNDEFINED_BITS);
                self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits])
            }
            Repr::Bool => self.i32_const(ctx, UNDEFINED_BOOL),
            Repr::Ref => self.i32_const(ctx, 0),
            Repr::I32 | Repr::I64 => bail!("optional fixed-width integer has no representation"),
        })
    }

    pub(crate) fn is_undefined(
        &mut self,
        ctx: &mut Context,
        value: Value,
        inner: TypeHandle,
    ) -> Result<Value> {
        Ok(match repr(ctx, inner)? {
            Repr::F64 => {
                let bits = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
                let undefined = self.i64_const(ctx, UNDEFINED_BITS);
                self.num(ctx, NumOpcode::I64Eq, vec![bits, undefined])
            }
            Repr::Bool => {
                let undefined = self.i32_const(ctx, UNDEFINED_BOOL);
                self.num(ctx, NumOpcode::I32Eq, vec![value, undefined])
            }
            Repr::Ref => self.num(ctx, NumOpcode::I32Eqz, vec![value]),
            Repr::I32 | Repr::I64 => bail!("optional fixed-width integer has no representation"),
        })
    }

    pub(crate) fn null(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Value> {
        Ok(match repr(ctx, ty)? {
            Repr::F64 => {
                let bits = self.i64_const(ctx, 0x7ff0_0000_0000_0002);
                self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits])
            }
            Repr::Bool => self.i32_const(ctx, 3),
            Repr::Ref => self.i32_const(ctx, 1),
            Repr::I32 | Repr::I64 => bail!("nullable fixed-width integer has no representation"),
        })
    }

    pub(crate) fn absence_test(
        &mut self,
        ctx: &mut Context,
        value: Value,
        ty: TypeHandle,
        null: bool,
    ) -> Result<Value> {
        let admitted = absence(ctx, ty).is_some_and(|a| {
            if null {
                a.admits_null()
            } else {
                a.admits_undefined()
            }
        });
        if !admitted {
            return Ok(self.i32_const(ctx, 0));
        }
        if !null {
            return self.is_undefined(ctx, value, ty);
        }
        let sentinel = self.null(ctx, ty)?;
        Ok(if repr(ctx, ty)? == Repr::F64 {
            let bits = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
            let sentinel = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![sentinel]);
            self.num(ctx, NumOpcode::I64Eq, vec![bits, sentinel])
        } else {
            self.num(ctx, NumOpcode::I32Eq, vec![value, sentinel])
        })
    }

    pub(crate) fn is_absent(
        &mut self,
        ctx: &mut Context,
        value: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        let undefined = self.absence_test(ctx, value, ty, false)?;
        let null = self.absence_test(ctx, value, ty, true)?;
        Ok(self.num(ctx, NumOpcode::I32Or, vec![undefined, null]))
    }

    pub(crate) fn optional_cast(
        &mut self,
        ctx: &mut Context,
        value: Value,
        source: TypeHandle,
        target: TypeHandle,
    ) -> Result<Value> {
        let from = absence(ctx, source)
            .ok_or_else(|| anyhow!("optional cast from a non-optional type"))?;
        let to =
            absence(ctx, target).ok_or_else(|| anyhow!("optional cast to a non-optional type"))?;
        if from == to {
            return Ok(value);
        }
        if from.admits_undefined() && !to.admits_undefined() {
            let rejected = self.absence_test(ctx, value, source, false)?;
            self.throw_if(ctx, rejected, ErrorKind::TypeError);
        }
        if from.admits_null() {
            let null = self.absence_test(ctx, value, source, true)?;
            if !to.admits_null() {
                self.throw_if(ctx, null, ErrorKind::TypeError);
            }
        }
        Ok(value)
    }
}
