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
    AggregateViewOp, ErrorKind, IsNullOp, IsUndefinedOp, NullOp, OmittedOp, OptionalCastOp,
    ToNumberOp, ToStringOp, UndefinedOp, UnwrapOp, WrapOp, absence,
};
use pliron::{
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_wasm::attrs::NumOpcode;

#[derive(Clone, Copy)]
enum AbsentKind {
    Undefined,
    Omitted,
    Null,
}

enum Sentinel {
    FloatBits(u64),
    Word(i32),
}

impl AbsentKind {
    fn tag(self) -> i32 {
        match self {
            Self::Undefined => numeric::UNDEFINED,
            Self::Omitted => numeric::OMITTED,
            Self::Null => numeric::NULL,
        }
    }

    fn sentinel(self, repr: Repr) -> Result<Sentinel> {
        let (float, boolean, reference) = match self {
            Self::Undefined => (UNDEFINED_BITS, UNDEFINED_BOOL, 0),
            Self::Omitted => (OMITTED_BITS, OMITTED_BOOL, OMITTED_REF),
            Self::Null => (0x7ff0_0000_0000_0002, 3, 1),
        };
        Ok(match repr {
            Repr::F64 => Sentinel::FloatBits(float),
            Repr::Bool => Sentinel::Word(boolean),
            Repr::Ref => Sentinel::Word(reference),
            Repr::I32 | Repr::I64 => bail!("optional fixed-width integer has no representation"),
        })
    }
}

impl FunctionLowering<'_, '_> {
    /// Lower an optional-value op; `false` if `op` is none.
    pub(crate) fn optional_op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<bool> {
        let absent = if Operation::get_op::<NullOp>(op, ctx).is_some() {
            Some(AbsentKind::Null)
        } else if Operation::get_op::<UndefinedOp>(op, ctx).is_some() {
            Some(AbsentKind::Undefined)
        } else if Operation::get_op::<OmittedOp>(op, ctx).is_some() {
            Some(AbsentKind::Omitted)
        } else {
            None
        };
        if let Some(kind) = absent {
            let ty = op.deref(ctx).get_result(0).get_type(ctx);
            let value = self.absent_value(ctx, ty, kind)?;
            self.define_value(ctx, op, value);
            return Ok(true);
        }
        if perry_mir::is_any::<(WrapOp, AggregateViewOp)>(ctx, op) {
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
        if perry_mir::is_any::<(IsUndefinedOp, IsNullOp)>(ctx, op) {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped_value(ctx, mir)?;
            let null = Operation::get_op::<IsNullOp>(op, ctx).is_some();
            let result = self.absence_test(ctx, value, mir.get_type(ctx), null)?;
            self.define(ctx, op, result);
            return Ok(true);
        }
        if Operation::get_op::<ToNumberOp>(op, ctx).is_some() {
            let mir = op.deref(ctx).get_operand(0);
            let value = self.mapped_value(ctx, mir)?;
            let undefined = self.absence_test(ctx, value, mir.get_type(ctx), false)?;
            let null = self.absence_test(ctx, value, mir.get_type(ctx), true)?;
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
            let absent = self.is_absent(ctx, value, mir.get_type(ctx))?;
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
        self.absent_value(ctx, ty, AbsentKind::Undefined)
    }

    fn absent_value(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        kind: AbsentKind,
    ) -> Result<Carriers<Value>> {
        if is_numeric_type(ctx, ty) {
            Ok(Carriers::Numeric {
                tag: self.i32_const(ctx, kind.tag()),
                payload: self.i64_const(ctx, 0),
            })
        } else {
            let value = match kind.sentinel(repr(ctx, ty)?)? {
                Sentinel::FloatBits(bits) => {
                    let bits = self.i64_const(ctx, bits);
                    self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits])
                }
                Sentinel::Word(word) => self.i32_const(ctx, word),
            };
            Ok(Carriers::Scalar(value))
        }
    }

    /// Whether `value` is the omitted marker; constant false when `ty` admits no `undefined`.
    pub(crate) fn is_omitted(
        &mut self,
        ctx: &mut Context,
        value: impl Into<Carriers<Value>>,
        ty: TypeHandle,
    ) -> Result<Value> {
        if !absence(ctx, ty).is_some_and(|a| a.admits_undefined()) {
            return Ok(self.i32_const(ctx, 0));
        }
        self.is_sentinel(ctx, value.into(), ty, AbsentKind::Omitted)
    }

    /// Whether `value` is `undefined` (`null == false`) or `null`; constant false when `ty`
    /// does not admit that absence. Omission counts as `undefined`.
    pub(crate) fn absence_test(
        &mut self,
        ctx: &mut Context,
        value: impl Into<Carriers<Value>>,
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
        let value = value.into();
        if null {
            self.is_sentinel(ctx, value, ty, AbsentKind::Null)
        } else {
            let explicit = self.is_sentinel(ctx, value, ty, AbsentKind::Undefined)?;
            let omitted = self.is_sentinel(ctx, value, ty, AbsentKind::Omitted)?;
            Ok(self.num(ctx, NumOpcode::I32Or, vec![explicit, omitted]))
        }
    }

    /// Whether `value` is `undefined`, omitted or `null`.
    pub(crate) fn is_absent(
        &mut self,
        ctx: &mut Context,
        value: impl Into<Carriers<Value>>,
        ty: TypeHandle,
    ) -> Result<Value> {
        let value = value.into();
        let undefined = self.absence_test(ctx, value, ty, false)?;
        let null = self.absence_test(ctx, value, ty, true)?;
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
            let rejected = self.absence_test(ctx, value, source, false)?;
            self.throw_if(ctx, rejected, ErrorKind::TypeError);
        }
        if from.admits_null() {
            let null = self.absence_test(ctx, value, source, true)?;
            if !to.admits_null() {
                self.throw_if(ctx, null, ErrorKind::TypeError);
            }
        }
        Ok(())
    }

    pub(crate) fn undefined(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Value> {
        self.absent_value(ctx, ty, AbsentKind::Undefined)?.scalar()
    }

    pub(crate) fn omitted(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Value> {
        self.absent_value(ctx, ty, AbsentKind::Omitted)?.scalar()
    }

    pub(crate) fn null(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Value> {
        self.absent_value(ctx, ty, AbsentKind::Null)?.scalar()
    }

    fn is_sentinel(
        &mut self,
        ctx: &mut Context,
        value: Carriers<Value>,
        ty: TypeHandle,
        kind: AbsentKind,
    ) -> Result<Value> {
        let (value, sentinel) = match value {
            Carriers::Numeric { tag, .. } => (tag, Sentinel::Word(kind.tag())),
            Carriers::Scalar(value) => (value, kind.sentinel(repr(ctx, ty)?)?),
        };
        Ok(match sentinel {
            Sentinel::FloatBits(bits) => {
                let value = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
                let sentinel = self.i64_const(ctx, bits);
                self.num(ctx, NumOpcode::I64Eq, vec![value, sentinel])
            }
            Sentinel::Word(word) => {
                let sentinel = self.i32_const(ctx, word);
                self.num(ctx, NumOpcode::I32Eq, vec![value, sentinel])
            }
        })
    }
}
