//! Lower semantic integer widths into Wasm's i32 and i64 representations.
use crate::function::FunctionLowering;
use anyhow::{Result, bail};
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        ops::ConstantOp,
        types::{FP64Type, IntegerType, Signedness},
    },
    context::Context,
    operation::Operation,
    r#type::TypeHandle,
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};
use pliron_wasm::ops::{LoadOp, NumOp};

/// Bounds guaranteed by the producing instruction, independent of source-level types.
fn produced_range(ctx: &Context, value: Value) -> Option<(i64, i64)> {
    let op = value.defining_op()?;
    if let Some(constant) = Operation::get_op::<ConstantOp>(op, ctx) {
        let attr = constant.get_attr_builtin_constant_value(ctx)?;
        let value = attr.downcast_ref::<IntegerAttr>()?.value().to_i64();
        return Some((value, value));
    }
    if let Some(load) = Operation::get_op::<LoadOp>(op, ctx) {
        return match *load.get_attr_wasm_load_kind(ctx)? {
            MemKind::I32S8 => Some((-128, 127)),
            MemKind::I32S16 => Some((-32768, 32767)),
            MemKind::I32U8 => Some((0, 255)),
            MemKind::I32U16 => Some((0, 65535)),
            _ => None,
        };
    }
    let num = Operation::get_op::<NumOp>(op, ctx)?;
    match *num.get_attr_wasm_num(ctx)? {
        NumOpcode::I32Extend8S => Some((-128, 127)),
        NumOpcode::I32Extend16S => Some((-32768, 32767)),
        NumOpcode::I32And => op.deref(ctx).operands().find_map(|value| {
            let constant = Operation::get_op::<ConstantOp>(value.defining_op()?, ctx)?;
            let attr = constant.get_attr_builtin_constant_value(ctx)?;
            let mask = attr.downcast_ref::<IntegerAttr>()?.value().to_i64();
            (mask >= 0).then_some((0, mask))
        }),
        _ => None,
    }
}

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
        if let Some((bits @ (8 | 16), signed)) = integer(ctx, ty)
            && let Some((lower, upper)) = produced_range(ctx, value)
        {
            let bounds = if signed {
                (-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
            } else {
                (0, (1 << bits) - 1)
            };
            if lower >= bounds.0 && upper <= bounds.1 {
                return value;
            }
        }
        match integer(ctx, ty) {
            Some((8, true)) => self.num(ctx, NumOpcode::I32Extend8S, vec![value]),
            Some((16, true)) => self.num(ctx, NumOpcode::I32Extend16S, vec![value]),
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
        let preserves_range = match (integer(ctx, source), integer(ctx, target)) {
            (Some((from, signed)), Some((to, target_signed))) => {
                (signed == target_signed && from <= to) || (!signed && target_signed && from < to)
            }
            _ => false,
        };
        Ok(if preserves_range {
            value
        } else {
            self.normalize_integer(ctx, value, target)
        })
    }
}
