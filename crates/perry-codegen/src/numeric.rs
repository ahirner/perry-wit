//! Lower semantic integer widths into Wasm's i32 and i64 representations.
use crate::function::FunctionLowering;
use anyhow::{Result, bail};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    r#type::TypeHandle,
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

pub(crate) fn integer(ctx: &Context, ty: TypeHandle) -> Option<(u32, bool)> {
    ty.deref(ctx)
        .downcast_ref::<IntegerType>()
        .map(|ty| (ty.width(), ty.signedness() != Signedness::Unsigned))
}

impl FunctionLowering<'_, '_> {
    pub(crate) fn normalize_integer(
        &mut self,
        ctx: &mut Context,
        value: Value,
        ty: TypeHandle,
    ) -> Value {
        match integer(ctx, ty) {
            Some((bits @ (8 | 16), true)) => {
                let shift = self.i32_const(ctx, (32 - bits) as i32);
                let shifted = self.num(ctx, NumOpcode::I32Shl, vec![value, shift]);
                self.num(ctx, NumOpcode::I32ShrS, vec![shifted, shift])
            }
            Some((bits @ (8 | 16), false)) => {
                let mask = self.i32_const(ctx, (1 << bits) - 1);
                self.num(ctx, NumOpcode::I32And, vec![value, mask])
            }
            _ => value,
        }
    }

    pub(crate) fn numeric_cast(
        &mut self,
        ctx: &mut Context,
        value: Value,
        source: TypeHandle,
        target: TypeHandle,
    ) -> Result<Value> {
        if source == target {
            return Ok(value);
        }
        if source.deref(ctx).is::<perry_mir::types::BigIntType>() {
            let magnitude = self.load(ctx, MemKind::I64, value, 0);
            let negative = self.load(ctx, MemKind::I32, value, 8);
            let zero = self.i64_const(ctx, 0);
            let inverted = self.num(ctx, NumOpcode::I64Sub, vec![zero, magnitude]);
            let raw = self.select(ctx, negative, inverted, magnitude);
            let source = IntegerType::get(ctx, 64, Signedness::Unsigned).into();
            return self.numeric_cast(ctx, raw, source, target);
        }
        if target.deref(ctx).is::<perry_mir::types::BigIntType>() {
            let (_, signed) = integer(ctx, source)
                .ok_or_else(|| anyhow::anyhow!("bigint boxing needs an integer"))?;
            let wide = IntegerType::get(
                ctx,
                64,
                if signed {
                    Signedness::Signed
                } else {
                    Signedness::Unsigned
                },
            )
            .into();
            let raw = self.numeric_cast(ctx, value, source, wide)?;
            return self.lift_integer(ctx, signed, target, raw);
        }
        let opcode = match (integer(ctx, source), integer(ctx, target)) {
            (Some((from, signed)), Some((to, _))) => {
                if from <= 32 && to == 64 {
                    Some(if signed {
                        NumOpcode::I64ExtendI32S
                    } else {
                        NumOpcode::I64ExtendI32U
                    })
                } else if from == 64 && to <= 32 {
                    Some(NumOpcode::I32WrapI64)
                } else {
                    None
                }
            }
            (Some((bits, signed)), None) if target.deref(ctx).is::<FP64Type>() => {
                Some(match (bits == 64, signed) {
                    (false, true) => NumOpcode::F64ConvertI32S,
                    (false, false) => NumOpcode::F64ConvertI32U,
                    (true, true) => NumOpcode::F64ConvertI64S,
                    (true, false) => NumOpcode::F64ConvertI64U,
                })
            }
            (None, Some((bits, signed))) if source.deref(ctx).is::<FP64Type>() => {
                Some(match (bits == 64, signed) {
                    (false, true) => NumOpcode::I32TruncSatF64S,
                    (false, false) => NumOpcode::I32TruncSatF64U,
                    (true, true) => NumOpcode::I64TruncSatF64S,
                    (true, false) => NumOpcode::I64TruncSatF64U,
                })
            }
            _ => bail!("unsupported numeric representation conversion"),
        };
        let value = opcode.map_or(value, |opcode| self.num(ctx, opcode, vec![value]));
        Ok(self.normalize_integer(ctx, value, target))
    }
}
