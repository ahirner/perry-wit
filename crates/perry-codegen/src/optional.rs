//! Optional values: the `undefined` and `null` sentinels of each representation, absence
//! tests and the optional ops.
use crate::{
    OMITTED_BOOL, OMITTED_REF, UNDEFINED_BOOL,
    function::FunctionLowering,
    repr::{Carriers, Repr, repr},
    values::is_numeric_type,
};
use anyhow::{Result, anyhow, bail};
use perry_abi::{OMITTED_BITS, UNDEFINED_BITS, layout::numeric};
use perry_mir::{
    attrs::ErrorKind,
    optional::{
        IsNullOp, IsUndefinedOp, NullOp, OmittedOp, OptionalCastOp, ToNumberOp, ToStringOp,
        UndefinedOp, UnwrapOp, WrapOp, absence,
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
            let value = self.null_value(ctx, null.get_result(ctx).get_type(ctx))?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if let Some(undefined) = Operation::get_op::<UndefinedOp>(op, ctx) {
            let value = self.undefined_value(ctx, undefined.get_result(ctx).get_type(ctx))?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if let Some(omitted) = Operation::get_op::<OmittedOp>(op, ctx) {
            let value = self.omitted_value(ctx, omitted.get_result(ctx).get_type(ctx))?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<WrapOp>(op, ctx).is_some()
            || Operation::get_op::<AggregateViewOp>(op, ctx).is_some()
        {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped_value(ctx, mir)?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<OptionalCastOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let source = mir.get_type(ctx);
            let target = op.deref(ctx).get_result(0).get_type(ctx);
            let value = self.mapped_value(ctx, mir)?;
            self.check_optional_cast(ctx, value, source, target)?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if Operation::get_op::<IsUndefinedOp>(op, ctx).is_some()
            || Operation::get_op::<IsNullOp>(op, ctx).is_some()
        {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped_value(ctx, mir)?;
            let null = Operation::get_op::<IsNullOp>(op, ctx).is_some();
            let result = self.absence_test_value(ctx, value, mir.get_type(ctx), null)?;
            self.define(ctx, op, result);
            return Ok(true);
        }
        if Operation::get_op::<ToNumberOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped_value(ctx, mir)?;
            let undefined = self.absence_test_value(ctx, value, mir.get_type(ctx), false)?;
            let null = self.absence_test_value(ctx, value, mir.get_type(ctx), true)?;
            let result = match value {
                Carriers::Scalar(v) => {
                    let nan = self.f64_const(ctx, f64::NAN);
                    let zero = self.f64_const(ctx, 0.0);
                    let number = self.select(ctx, null, zero, v);
                    Carriers::Scalar(self.select(ctx, undefined, nan, number))
                }
                Carriers::Numeric { tag, payload } => {
                    let f_tag = self.i32_const(ctx, numeric::FLOAT);
                    let nan_bits = self.i64_const(ctx, f64::NAN.to_bits());
                    let zero_bits = self.i64_const(ctx, 0);
                    let t1 = self.select(ctx, null, f_tag, tag);
                    let p1 = self.select(ctx, null, zero_bits, payload);
                    let t2 = self.select(ctx, undefined, f_tag, t1);
                    let p2 = self.select(ctx, undefined, nan_bits, p1);
                    Carriers::Numeric {
                        tag: t2,
                        payload: p2,
                    }
                }
            };
            self.define_value(ctx, op, result);
            return Ok(true);
        }
        if Operation::get_op::<UnwrapOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped_value(ctx, mir)?;
            let absent = self.is_absent_value(ctx, value, mir.get_type(ctx))?;
            self.throw_if(ctx, absent, ErrorKind::TypeError);
            self.define_value(ctx, op, value);
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

    /// `undefined` in the carriers of `ty`.
    pub(crate) fn undefined_value(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
    ) -> Result<Carriers<Value>> {
        self.absent_value(ctx, ty, numeric::UNDEFINED, Self::undefined)
    }

    fn omitted_value(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Carriers<Value>> {
        self.absent_value(ctx, ty, numeric::OMITTED, Self::omitted)
    }

    fn null_value(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Carriers<Value>> {
        self.absent_value(ctx, ty, numeric::NULL, Self::null)
    }

    /// An absent value of `ty`: the tag `numeric_tag` for numeric alternatives, otherwise the
    /// `scalar` sentinel.
    fn absent_value(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        numeric_tag: i32,
        scalar: fn(&mut Self, &mut Context, TypeHandle) -> Result<Value>,
    ) -> Result<Carriers<Value>> {
        if is_numeric_type(ctx, ty) {
            Ok(Carriers::Numeric {
                tag: self.i32_const(ctx, numeric_tag),
                payload: self.i64_const(ctx, 0),
            })
        } else {
            scalar(self, ctx, ty).map(Carriers::Scalar)
        }
    }

    /// Whether `value` is the omitted marker; constant false when `ty` admits no `undefined`.
    pub(crate) fn is_omitted_value(
        &mut self,
        ctx: &mut Context,
        value: Carriers<Value>,
        ty: TypeHandle,
    ) -> Result<Value> {
        if !absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
            return Ok(self.i32_const(ctx, 0));
        }
        match value {
            Carriers::Numeric { tag, .. } => {
                let omitted = self.i32_const(ctx, numeric::OMITTED);
                Ok(self.num(ctx, NumOpcode::I32Eq, vec![tag, omitted]))
            }
            Carriers::Scalar(value) => self.is_omitted(ctx, value, ty),
        }
    }

    /// Whether `value` is `undefined` (`null == false`) or `null`; constant false when `ty`
    /// does not admit that absence. Omission counts as `undefined`.
    pub(crate) fn absence_test_value(
        &mut self,
        ctx: &mut Context,
        value: Carriers<Value>,
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
        match (value, null) {
            (Carriers::Numeric { tag, .. }, false) => {
                let undef = self.i32_const(ctx, numeric::UNDEFINED);
                let explicit = self.num(ctx, NumOpcode::I32Eq, vec![tag, undef]);
                let omitted = self.is_omitted_value(ctx, value, ty)?;
                Ok(self.num(ctx, NumOpcode::I32Or, vec![explicit, omitted]))
            }
            (Carriers::Numeric { tag, .. }, true) => {
                let null_tag = self.i32_const(ctx, numeric::NULL);
                Ok(self.num(ctx, NumOpcode::I32Eq, vec![tag, null_tag]))
            }
            (Carriers::Scalar(value), false) => self.is_undefined(ctx, value, ty),
            (Carriers::Scalar(value), true) => {
                let sentinel = self.null(ctx, ty)?;
                Ok(if repr(ctx, ty)? == Repr::F64 {
                    let bits = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
                    let sentinel = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![sentinel]);
                    self.num(ctx, NumOpcode::I64Eq, vec![bits, sentinel])
                } else {
                    self.num(ctx, NumOpcode::I32Eq, vec![value, sentinel])
                })
            }
        }
    }

    /// Whether `value` is `undefined`, omitted or `null`.
    pub(crate) fn is_absent_value(
        &mut self,
        ctx: &mut Context,
        value: Carriers<Value>,
        ty: TypeHandle,
    ) -> Result<Value> {
        let undefined = self.absence_test_value(ctx, value, ty, false)?;
        let null = self.absence_test_value(ctx, value, ty, true)?;
        Ok(self.num(ctx, NumOpcode::I32Or, vec![undefined, null]))
    }

    /// Throw a `TypeError` when `value` holds an absence `source` admits but `target` does
    /// not. Both types share a representation, so the value itself is reused unchanged.
    pub(crate) fn check_optional_cast(
        &mut self,
        ctx: &mut Context,
        value: Carriers<Value>,
        source: TypeHandle,
        target: TypeHandle,
    ) -> Result<()> {
        let from = absence(ctx, source)
            .ok_or_else(|| anyhow!("optional cast from a non-optional type"))?;
        let to =
            absence(ctx, target).ok_or_else(|| anyhow!("optional cast to a non-optional type"))?;
        if from == to {
            return Ok(());
        }
        if from.admits_undefined() && !to.admits_undefined() {
            let rejected = self.absence_test_value(ctx, value, source, false)?;
            self.throw_if(ctx, rejected, ErrorKind::TypeError);
        }
        if from.admits_null() {
            let null = self.absence_test_value(ctx, value, source, true)?;
            if !to.admits_null() {
                self.throw_if(ctx, null, ErrorKind::TypeError);
            }
        }
        Ok(())
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

    pub(crate) fn omitted(&mut self, ctx: &mut Context, inner: TypeHandle) -> Result<Value> {
        Ok(match repr(ctx, inner)? {
            Repr::F64 => {
                let bits = self.i64_const(ctx, OMITTED_BITS);
                self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits])
            }
            Repr::Bool => self.i32_const(ctx, OMITTED_BOOL),
            Repr::Ref => self.i32_const(ctx, OMITTED_REF),
            Repr::I32 | Repr::I64 => bail!("optional fixed-width integer has no representation"),
        })
    }

    pub(crate) fn is_omitted(
        &mut self,
        ctx: &mut Context,
        value: Value,
        inner: TypeHandle,
    ) -> Result<Value> {
        if !absence(ctx, inner).is_some_and(|a| a.admits_undefined()) {
            return Ok(self.i32_const(ctx, 0));
        }
        Ok(match repr(ctx, inner)? {
            Repr::F64 => {
                let bits = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
                let omitted = self.i64_const(ctx, OMITTED_BITS);
                self.num(ctx, NumOpcode::I64Eq, vec![bits, omitted])
            }
            Repr::Bool => {
                let omitted = self.i32_const(ctx, OMITTED_BOOL);
                self.num(ctx, NumOpcode::I32Eq, vec![value, omitted])
            }
            Repr::Ref => {
                let omitted = self.i32_const(ctx, OMITTED_REF);
                self.num(ctx, NumOpcode::I32Eq, vec![value, omitted])
            }
            Repr::I32 | Repr::I64 => bail!("optional fixed-width integer has no representation"),
        })
    }

    pub(crate) fn is_undefined(
        &mut self,
        ctx: &mut Context,
        value: Value,
        inner: TypeHandle,
    ) -> Result<Value> {
        let explicit = match repr(ctx, inner)? {
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
        };
        let omitted = self.is_omitted(ctx, value, inner)?;
        Ok(self.num(ctx, NumOpcode::I32Or, vec![explicit, omitted]))
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

    /// [`Self::absence_test_value`] for a scalar representation.
    pub(crate) fn absence_test(
        &mut self,
        ctx: &mut Context,
        value: Value,
        ty: TypeHandle,
        null: bool,
    ) -> Result<Value> {
        self.absence_test_value(ctx, Carriers::Scalar(value), ty, null)
    }

    /// [`Self::is_absent_value`] for a scalar representation.
    pub(crate) fn is_absent(
        &mut self,
        ctx: &mut Context,
        value: Value,
        ty: TypeHandle,
    ) -> Result<Value> {
        self.is_absent_value(ctx, Carriers::Scalar(value), ty)
    }
}
