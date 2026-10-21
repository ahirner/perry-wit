//! Lower numeric conversions, semantic widths and lossless tagged alternatives.
mod math;

use crate::{
    function::FunctionLowering,
    repr::{Carriers, Repr, i32_type, repr},
    runtime::Runtime,
    values::is_numeric_type,
};
use anyhow::{Result, bail, ensure};
use perry_abi::layout::numeric::{FLOAT, NULL, OMITTED, SIGNED, U8, U64, UNDEFINED, UNSIGNED};
use perry_mir::{BinaryKind, CompareKind, NumericConstant, NumericType};
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        ops::ConstantOp,
        types::{FP64Type, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    operation::Operation,
    printable::Printable,
    r#type::{TypeHandle, Typed},
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

/// The fixed-width tag (`U8` through `S64`) of a `bits`-wide integer: unsigned tags are even,
/// and each wider width adds two.
fn fixed_width_tag(bits: u32, signed: bool) -> Result<i32> {
    let width = match bits {
        8 => 0,
        16 => 1,
        32 => 2,
        64 => 3,
        other => bail!("{other}-bit integers have no numeric tag"),
    };
    Ok(U8 + 2 * width + i32::from(signed))
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
        if source.deref(ctx).is::<perry_mir::BigIntType>() {
            let magnitude = self.load(ctx, MemKind::I64, value, 0);
            let negative = self.load(ctx, MemKind::I32, value, 8);
            let raw = self.apply_integer_sign(ctx, magnitude, negative);
            let source = IntegerType::get(ctx, 64, Signedness::Unsigned).into();
            return self.numeric_cast(ctx, raw, source, target);
        }
        if target.deref(ctx).is::<perry_mir::BigIntType>() {
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
            _ => bail!(
                "unsupported numeric representation conversion: source={}, target={}",
                source.disp(ctx),
                target.disp(ctx)
            ),
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

    pub(super) fn numeric_constant(
        &mut self,
        ctx: &mut Context,
        number: NumericConstant,
    ) -> Carriers<Value> {
        let (tag, bits) = match number {
            NumericConstant::Signed(value) => (SIGNED, value as u64),
            NumericConstant::Unsigned(value) => (UNSIGNED, value),
            NumericConstant::Float(bits) => (FLOAT, bits),
        };
        Carriers::Numeric {
            tag: self.i32_const(ctx, tag),
            payload: self.i64_const(ctx, bits),
        }
    }

    pub(crate) fn cast_value(
        &mut self,
        ctx: &mut Context,
        value: Carriers<Value>,
        source: TypeHandle,
        target: TypeHandle,
    ) -> Result<Carriers<Value>> {
        if source == target || (is_numeric_type(ctx, source) && is_numeric_type(ctx, target)) {
            return Ok(value);
        }
        if is_numeric_type(ctx, target) {
            let (tag, payload) = self.tag_scalar(ctx, value.scalar()?, source)?;
            return Ok(Carriers::Numeric { tag, payload });
        }
        let value = match value {
            Carriers::Scalar(value) => self.numeric_cast(ctx, value, source, target)?,
            Carriers::Numeric { tag, payload } => self.untag_numeric(ctx, tag, payload, target)?,
        };
        Ok(Carriers::Scalar(value))
    }

    /// The tag and payload of operand `index`, tagging a fixed-width or float operand.
    pub(super) fn numeric_operand(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        index: usize,
    ) -> Result<(Value, Value)> {
        let operand = op.deref(ctx).get_operand(index);
        let source = operand.get_type(ctx);
        let value = self.mapped_value(ctx, operand)?;
        if is_numeric_type(ctx, source) {
            value.numeric()
        } else {
            self.tag_scalar(ctx, value.scalar()?, source)
        }
    }

    /// Tag and payload of `value`, whose `source` type is a fixed-width integer, a float or an
    /// optional float.
    fn tag_scalar(
        &mut self,
        ctx: &mut Context,
        value: Value,
        source: TypeHandle,
    ) -> Result<(Value, Value)> {
        if crate::repr::optional_inner(ctx, source).is_some() {
            let undef = self.absence_test(ctx, value, source, false)?;
            let null = self.absence_test(ctx, value, source, true)?;
            let omit = self.is_omitted(ctx, value, source)?;
            let not_omit = self.num(ctx, NumOpcode::I32Eqz, vec![omit]);
            let undef_not_omit = self.num(ctx, NumOpcode::I32And, vec![undef, not_omit]);
            let tag_float = self.i32_const(ctx, FLOAT);
            let tag_null = self.i32_const(ctx, NULL);
            let tag_undef = self.i32_const(ctx, UNDEFINED);
            let tag_omit = self.i32_const(ctx, OMITTED);
            let tag = self.select(ctx, null, tag_null, tag_float);
            let tag = self.select(ctx, undef_not_omit, tag_undef, tag);
            let tag = self.select(ctx, omit, tag_omit, tag);
            // `undefined` already covers omission.
            let absent = self.num(ctx, NumOpcode::I32Or, vec![undef, null]);
            let float_bits = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
            let zero = self.i64_const(ctx, 0);
            let payload = self.select(ctx, absent, zero, float_bits);
            return Ok((tag, payload));
        }
        let (tag, payload) = if let Some((bits, signed)) = integer(ctx, source) {
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
            (
                fixed_width_tag(bits, signed)?,
                self.numeric_cast(ctx, value, source, wide)?,
            )
        } else {
            (
                FLOAT,
                self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]),
            )
        };
        Ok((self.i32_const(ctx, tag), payload))
    }

    /// Convert present numeric payloads with nontrapping conversions and select the absent
    /// sentinel when the tag indicates absence.
    fn untag_numeric(
        &mut self,
        ctx: &mut Context,
        tag: Value,
        payload: Value,
        target: TypeHandle,
    ) -> Result<Value> {
        let inner = crate::repr::optional_inner(ctx, target);
        let scalar = inner.unwrap_or(target);
        let float_type = FP64Type::get(ctx).into();
        let float = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![payload]);
        let float = self.numeric_cast(ctx, float, float_type, scalar)?;
        let signed_type = IntegerType::get(ctx, 64, Signedness::Signed).into();
        let mut integer = self.numeric_cast(ctx, payload, signed_type, scalar)?;
        if scalar == float_type {
            let unsigned_type = IntegerType::get(ctx, 64, Signedness::Unsigned).into();
            let unsigned = self.numeric_cast(ctx, payload, unsigned_type, scalar)?;
            let one = self.i32_const(ctx, 1);
            let signed = self.num(ctx, NumOpcode::I32And, vec![tag, one]);
            integer = self.select(ctx, signed, integer, unsigned);
        }
        let is_float = self.num(ctx, NumOpcode::I32Eqz, vec![tag]);
        let present = self.select(ctx, is_float, float, integer);
        let Some(_) = inner else { return Ok(present) };
        let absent = self.is_absent_tag(ctx, tag);
        let null_tag = self.i32_const(ctx, NULL);
        let omit_tag = self.i32_const(ctx, OMITTED);
        let is_null = self.num(ctx, NumOpcode::I32Eq, vec![tag, null_tag]);
        let is_omit = self.num(ctx, NumOpcode::I32Eq, vec![tag, omit_tag]);
        let undef_val = self.undefined(ctx, target)?;
        let null_val = self.null(ctx, target)?;
        let omit_val = self.omitted(ctx, target)?;
        let absent_val = self.select(ctx, is_null, null_val, undef_val);
        let absent_val = self.select(ctx, is_omit, omit_val, absent_val);
        Ok(self.select(ctx, absent, absent_val, present))
    }

    /// Compute the `br_table` index of a present tag (`FLOAT`, `SIGNED`, or `UNSIGNED`).
    /// Integer tags, including fixed-width variants, are odd when signed, and callers filter
    /// absent tags beforehand.
    pub(crate) fn canonical_numeric_tag(&mut self, ctx: &mut Context, tag: Value) -> Value {
        let float_tag = self.i32_const(ctx, FLOAT);
        let is_float = self.num(ctx, NumOpcode::I32Eq, vec![tag, float_tag]);
        let one = self.i32_const(ctx, 1);
        let two = self.i32_const(ctx, 2);
        let parity = self.num(ctx, NumOpcode::I32And, vec![tag, one]);
        let int_tag = self.num(ctx, NumOpcode::I32Sub, vec![two, parity]);
        self.select(ctx, is_float, float_tag, int_tag)
    }

    /// Whether `tag` is `undefined`, `null` or omitted.
    fn is_absent_tag(&mut self, ctx: &mut Context, tag: Value) -> Value {
        let undef_tag = self.i32_const(ctx, UNDEFINED);
        let omit_tag = self.i32_const(ctx, OMITTED);
        let ge_undef = self.num(ctx, NumOpcode::I32GeU, vec![tag, undef_tag]);
        let le_omit = self.num(ctx, NumOpcode::I32LeU, vec![tag, omit_tag]);
        self.num(ctx, NumOpcode::I32And, vec![ge_undef, le_omit])
    }

    /// Whether `tag` names a fixed integer width (`U8` through `S64`).
    fn is_fixed_tag(&mut self, ctx: &mut Context, tag: Value) -> Value {
        let u8_tag = self.i32_const(ctx, U8);
        self.num(ctx, NumOpcode::I32GeU, vec![tag, u8_tag])
    }

    pub(super) fn binary(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        kind: BinaryKind,
    ) -> Result<Carriers<Value>> {
        let ty = op.deref(ctx).get_result(0).get_type(ctx);
        if !ty.deref(ctx).is::<NumericType>() {
            let operands = self.operands(ctx, op)?;
            let divisor = crate::repr::nonzero_i32_constant(ctx, op.deref(ctx).get_operand(1));
            return self
                .scalar_binary(ctx, kind, ty, operands, divisor)
                .map(Carriers::Scalar);
        }
        let left = op.deref(ctx).get_operand(0);
        let right = op.deref(ctx).get_operand(1);
        let left = self.mapped_value(ctx, left)?;
        let right = self.mapped_value(ctx, right)?;
        let ((lt, lp), (rt, rp)) = (left.numeric()?, right.numeric()?);
        let done = self.new_block(ctx, crate::values::signature(ctx, [ty])?);
        let floats = self.new_block(ctx, vec![]);
        if kind == BinaryKind::Div {
            self.br(ctx, floats, vec![]);
        } else {
            let integers = self.new_block(ctx, vec![]);
            let lf = self.num(ctx, NumOpcode::I32Eqz, vec![lt]);
            let rf = self.num(ctx, NumOpcode::I32Eqz, vec![rt]);
            let floating = self.num(ctx, NumOpcode::I32Or, vec![lf, rf]);
            self.cond_br(ctx, floating, (floats, vec![]), (integers, vec![]));
            self.block = integers;
            let one = self.i32_const(ctx, 1);
            let l_odd = self.num(ctx, NumOpcode::I32And, vec![lt, one]);
            let r_odd = self.num(ctx, NumOpcode::I32And, vec![rt, one]);
            let lu = self.num(ctx, NumOpcode::I32Eqz, vec![l_odd]);
            let ru = self.num(ctx, NumOpcode::I32Eqz, vec![r_odd]);
            let is_unsigned = self.num(ctx, NumOpcode::I32Or, vec![lu, ru]);
            let unsigned = self.i32_const(ctx, UNSIGNED);
            let signed = self.i32_const(ctx, SIGNED);
            let tag = self.select(ctx, is_unsigned, unsigned, signed);
            let l_fixed = self.is_fixed_tag(ctx, lt);
            let r_fixed = self.is_fixed_tag(ctx, rt);
            let any_fixed = self.num(ctx, NumOpcode::I32Or, vec![l_fixed, r_fixed]);
            let left_wider = self.num(ctx, NumOpcode::I32GtU, vec![lt, rt]);
            let wider_tag = self.select(ctx, left_wider, lt, rt);
            if kind == BinaryKind::Rem {
                let zero = self.num(ctx, NumOpcode::I64Eqz, vec![rp]);
                let nan = self.new_block(ctx, vec![]);
                let nonzero = self.new_block(ctx, vec![]);
                self.cond_br(ctx, zero, (nan, vec![]), (nonzero, vec![]));
                self.block = nan;
                let nan = self.numeric_constant(ctx, NumericConstant::Float(f64::NAN.to_bits()));
                self.br(ctx, done, nan.iter().collect());
                self.block = nonzero;
                let l_is_neg = self.numeric_negative(ctx, lt, lp);
                let r_is_neg = self.numeric_negative(ctx, rt, rp);
                let abs_lp = self.apply_integer_sign(ctx, lp, l_is_neg);
                let abs_rp = self.apply_integer_sign(ctx, rp, r_is_neg);
                let rem_u = self.num(ctx, NumOpcode::I64RemU, vec![abs_lp, abs_rp]);
                let rem_zero = self.num(ctx, NumOpcode::I64Eqz, vec![rem_u]);
                let is_neg_zero = self.num(ctx, NumOpcode::I32And, vec![l_is_neg, rem_zero]);

                let normal_val = self.apply_integer_sign(ctx, rem_u, l_is_neg);

                let rem_u_tag = self.select(ctx, any_fixed, wider_tag, unsigned);
                let pos_tag = self.select(ctx, lu, rem_u_tag, signed);
                let normal_tag = self.select(ctx, l_is_neg, signed, pos_tag);

                let float = self.i32_const(ctx, FLOAT);
                let bits = self.i64_const(ctx, (-0.0f64).to_bits());
                let tag = self.select(ctx, is_neg_zero, float, normal_tag);
                let value = self.select(ctx, is_neg_zero, bits, normal_val);
                self.br(ctx, done, vec![tag, value]);
            } else {
                let integer_type = IntegerType::get(ctx, 64, Signedness::Unsigned).into();
                let raw_value = self.scalar_binary(ctx, kind, integer_type, vec![lp, rp], None)?;
                let both_fixed = self.num(ctx, NumOpcode::I32And, vec![l_fixed, r_fixed]);
                let same_sign = self.num(ctx, NumOpcode::I32Eq, vec![l_odd, r_odd]);
                let unsigned_tag = self.select(ctx, l_odd, rt, lt);
                // The next wider signed tag is three above an unsigned tag. U64 cannot widen.
                let three = self.i32_const(ctx, 3);
                let widened = self.num(ctx, NumOpcode::I32Add, vec![unsigned_tag, three]);
                let already_wider = self.num(ctx, NumOpcode::I32GtU, vec![wider_tag, widened]);
                let mixed = self.select(ctx, already_wider, wider_tag, widened);
                let u64_tag = self.i32_const(ctx, U64);
                let full_width = self.num(ctx, NumOpcode::I32Eq, vec![unsigned_tag, u64_tag]);
                let mixed = self.select(ctx, full_width, u64_tag, mixed);
                let promoted = self.select(ctx, same_sign, wider_tag, mixed);
                let single = self.select(ctx, l_fixed, lt, rt);
                let mut fixed_tag = self.select(ctx, both_fixed, promoted, single);
                let s = self.num(ctx, NumOpcode::I32And, vec![fixed_tag, one]);
                if Operation::get_op::<perry_mir::BinaryOp>(op, ctx)
                    .is_some_and(|b| b.is_unconstrained(ctx))
                {
                    fixed_tag = self.num(ctx, NumOpcode::I32Add, vec![u64_tag, s]);
                }
                let u8_tag = self.i32_const(ctx, U8);
                let width = self.num(ctx, NumOpcode::I32Sub, vec![fixed_tag, u8_tag]);
                let w = self.num(ctx, NumOpcode::I32ShrU, vec![width, one]);
                let eight = self.i32_const(ctx, 8);
                let bits = self.num(ctx, NumOpcode::I32Shl, vec![eight, w]);
                let sixty_four = self.i32_const(ctx, 64);
                let shift32 = self.num(ctx, NumOpcode::I32Sub, vec![sixty_four, bits]);
                let shift64 = self.num(ctx, NumOpcode::I64ExtendI32U, vec![shift32]);
                let shifted = self.num(ctx, NumOpcode::I64Shl, vec![raw_value, shift64]);
                let wrapped_s = self.num(ctx, NumOpcode::I64ShrS, vec![shifted, shift64]);
                let wrapped_u = self.num(ctx, NumOpcode::I64ShrU, vec![shifted, shift64]);
                let wrapped_value = self.select(ctx, s, wrapped_s, wrapped_u);

                let (unfixed_tag, unfixed_value) = if kind == BinaryKind::Mul {
                    let left_zero = self.num(ctx, NumOpcode::I64Eqz, vec![lp]);
                    let right_zero = self.num(ctx, NumOpcode::I64Eqz, vec![rp]);
                    let zero = self.num(ctx, NumOpcode::I32Or, vec![left_zero, right_zero]);
                    let signs = self.num(ctx, NumOpcode::I64Xor, vec![lp, rp]);
                    let empty = self.i64_const(ctx, 0);
                    let negative = self.num(ctx, NumOpcode::I64LtS, vec![signs, empty]);
                    let signed = self.num(ctx, NumOpcode::I32Eqz, vec![is_unsigned]);
                    let negative_zero = self.num(ctx, NumOpcode::I32And, vec![zero, negative]);
                    let negative_zero =
                        self.num(ctx, NumOpcode::I32And, vec![negative_zero, signed]);
                    let float = self.i32_const(ctx, FLOAT);
                    let bits = self.i64_const(ctx, (-0.0f64).to_bits());
                    (
                        self.select(ctx, negative_zero, float, tag),
                        self.select(ctx, negative_zero, bits, raw_value),
                    )
                } else {
                    (tag, raw_value)
                };
                let final_tag = self.select(ctx, any_fixed, fixed_tag, unfixed_tag);
                let final_value = self.select(ctx, any_fixed, wrapped_value, unfixed_value);
                self.br(ctx, done, vec![final_tag, final_value]);
            }
        }
        self.block = floats;
        let float_type = FP64Type::get(ctx).into();
        let lhs = self.cast_value(ctx, left, ty, float_type)?.scalar()?;
        let rhs = self.cast_value(ctx, right, ty, float_type)?.scalar()?;
        let value = self.scalar_binary(ctx, kind, float_type, vec![lhs, rhs], None)?;
        let tag = self.i32_const(ctx, FLOAT);
        let payload = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]);
        self.br(ctx, done, vec![tag, payload]);
        self.block = done;
        Ok(Carriers::Numeric {
            tag: done.deref(ctx).get_argument(0),
            payload: done.deref(ctx).get_argument(1),
        })
    }

    fn scalar_binary(
        &mut self,
        ctx: &mut Context,
        kind: BinaryKind,
        ty: TypeHandle,
        operands: Vec<Value>,
        divisor: Option<i32>,
    ) -> Result<Value> {
        if ty.deref(ctx).is::<perry_mir::BigIntType>() {
            let runtime = match kind {
                BinaryKind::Add => Runtime::StdBigintAdd,
                BinaryKind::Sub => Runtime::StdBigintSub,
                _ => bail!("bigint {kind:?} is not supported yet"),
            };
            let value = self.runtime(ctx, runtime, operands);
            let status = self.runtime(ctx, Runtime::Status, vec![]);
            self.throw_status_if(ctx, status, status);
            return Ok(value);
        }
        if let Some((bits, signed)) = integer(ctx, ty) {
            let opcode = match (kind, bits == 64, signed) {
                (BinaryKind::Add, false, _) => NumOpcode::I32Add,
                (BinaryKind::Sub, false, _) => NumOpcode::I32Sub,
                (BinaryKind::Mul, false, _) => NumOpcode::I32Mul,
                (BinaryKind::Div, false, true) => NumOpcode::I32DivS,
                (BinaryKind::Div, false, false) => NumOpcode::I32DivU,
                (BinaryKind::Rem, false, true) => NumOpcode::I32RemS,
                (BinaryKind::Rem, false, false) => NumOpcode::I32RemU,
                (BinaryKind::Add, true, _) => NumOpcode::I64Add,
                (BinaryKind::Sub, true, _) => NumOpcode::I64Sub,
                (BinaryKind::Mul, true, _) => NumOpcode::I64Mul,
                (BinaryKind::Div, true, true) => NumOpcode::I64DivS,
                (BinaryKind::Div, true, false) => NumOpcode::I64DivU,
                (BinaryKind::Rem, true, true) => NumOpcode::I64RemS,
                (BinaryKind::Rem, true, false) => NumOpcode::I64RemU,
            };
            let value = self.num(ctx, opcode, operands);
            return Ok(self.normalize_integer(ctx, value, ty));
        }
        if !ty.deref(ctx).is::<FP64Type>() {
            bail!("{kind:?} requires a resolved numeric representation");
        }
        Ok(match kind {
            BinaryKind::Rem => self.remainder(ctx, operands, divisor),
            BinaryKind::Add => self.num(ctx, NumOpcode::F64Add, operands),
            BinaryKind::Sub => self.num(ctx, NumOpcode::F64Sub, operands),
            BinaryKind::Mul => self.num(ctx, NumOpcode::F64Mul, operands),
            BinaryKind::Div => self.num(ctx, NumOpcode::F64Div, operands),
        })
    }

    pub(super) fn remainder(
        &mut self,
        ctx: &mut Context,
        operands: Vec<Value>,
        constant: Option<i32>,
    ) -> Value {
        let (dividend, divisor) = (operands[0], operands[1]);
        let a = self.num(ctx, NumOpcode::I32TruncSatF64S, vec![dividend]);
        let back = self.num(ctx, NumOpcode::F64ConvertI32S, vec![a]);
        let mut fast = self.num(ctx, NumOpcode::F64Eq, vec![back, dividend]);
        let b = match constant {
            Some(constant) => self.i32_const(ctx, constant),
            None => {
                let b = self.num(ctx, NumOpcode::I32TruncSatF64S, vec![divisor]);
                let back = self.num(ctx, NumOpcode::F64ConvertI32S, vec![b]);
                let exact = self.num(ctx, NumOpcode::F64Eq, vec![back, divisor]);
                fast = self.num(ctx, NumOpcode::I32And, vec![fast, exact]);
                // A zero divisor gives NaN (`fmod`); `i32.rem_s` would trap.
                let zero = self.num(ctx, NumOpcode::I32Eqz, vec![b]);
                let nonzero = self.num(ctx, NumOpcode::I32Eqz, vec![zero]);
                fast = self.num(ctx, NumOpcode::I32And, vec![fast, nonzero]);
                b
            }
        };
        let f64 = FP64Type::get(ctx).into();
        let integer = self.new_block(ctx, vec![]);
        let general = self.new_block(ctx, vec![]);
        let join = self.new_block(ctx, vec![f64]);
        self.cond_br(ctx, fast, (integer, vec![]), (general, vec![]));
        self.block = integer;
        let rem = self.num(ctx, NumOpcode::I32RemS, vec![a, b]);
        let rem = self.num(ctx, NumOpcode::F64ConvertI32S, vec![rem]);
        let rem = self.num(ctx, NumOpcode::F64Copysign, vec![rem, dividend]);
        self.br(ctx, join, vec![rem]);
        self.block = general;
        let rem = self.runtime(ctx, Runtime::F64Rem, vec![dividend, divisor]);
        self.br(ctx, join, vec![rem]);
        self.block = join;
        join.deref(ctx).get_argument(0)
    }

    /// Compare two scalars of MIR type `ty`: floats and integers by value, references (and
    /// optional references) by address, which only answers identity.
    pub(super) fn scalar_comparison(
        &mut self,
        ctx: &mut Context,
        kind: CompareKind,
        ty: TypeHandle,
        left: Value,
        right: Value,
    ) -> Result<Value> {
        Ok(if left.get_type(ctx).deref(ctx).is::<FP64Type>() {
            let opcode = match kind {
                CompareKind::Lt => NumOpcode::F64Lt,
                CompareKind::Le => NumOpcode::F64Le,
                CompareKind::Gt => NumOpcode::F64Gt,
                CompareKind::Ge => NumOpcode::F64Ge,
                CompareKind::Eq => NumOpcode::F64Eq,
                CompareKind::Ne => NumOpcode::F64Ne,
            };
            self.num(ctx, opcode, vec![left, right])
        } else {
            let scalar = crate::repr::optional_inner(ctx, ty).unwrap_or(ty);
            let (bits, signed) = match integer(ctx, scalar) {
                Some(integer) => integer,
                None => {
                    ensure!(
                        repr(ctx, scalar)? == Repr::Ref
                            && matches!(kind, CompareKind::Eq | CompareKind::Ne),
                        "comparison {kind:?} is not supported on {}",
                        ty.disp(ctx)
                    );
                    (32, false)
                }
            };
            let opcode = match (kind, bits == 64, signed) {
                (CompareKind::Lt, false, true) => NumOpcode::I32LtS,
                (CompareKind::Le, false, true) => NumOpcode::I32LeS,
                (CompareKind::Gt, false, true) => NumOpcode::I32GtS,
                (CompareKind::Ge, false, true) => NumOpcode::I32GeS,
                (CompareKind::Lt, false, false) => NumOpcode::I32LtU,
                (CompareKind::Le, false, false) => NumOpcode::I32LeU,
                (CompareKind::Gt, false, false) => NumOpcode::I32GtU,
                (CompareKind::Ge, false, false) => NumOpcode::I32GeU,
                (CompareKind::Eq, false, _) => NumOpcode::I32Eq,
                (CompareKind::Ne, false, _) => NumOpcode::I32Ne,
                (CompareKind::Lt, true, true) => NumOpcode::I64LtS,
                (CompareKind::Le, true, true) => NumOpcode::I64LeS,
                (CompareKind::Gt, true, true) => NumOpcode::I64GtS,
                (CompareKind::Ge, true, true) => NumOpcode::I64GeS,
                (CompareKind::Lt, true, false) => NumOpcode::I64LtU,
                (CompareKind::Le, true, false) => NumOpcode::I64LeU,
                (CompareKind::Gt, true, false) => NumOpcode::I64GtU,
                (CompareKind::Ge, true, false) => NumOpcode::I64GeU,
                (CompareKind::Eq, true, _) => NumOpcode::I64Eq,
                (CompareKind::Ne, true, _) => NumOpcode::I64Ne,
            };
            self.num(ctx, opcode, vec![left, right])
        })
    }

    pub(super) fn numeric_comparison(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        kind: CompareKind,
    ) -> Result<Value> {
        let (left, right) = (op.deref(ctx).get_operand(0), op.deref(ctx).get_operand(1));
        let (at, ap) = self.mapped_value(ctx, left)?.numeric()?;
        let (bt, bp) = self.mapped_value(ctx, right)?.numeric()?;
        let i32 = i32_type(ctx);
        let done = self.new_block(ctx, vec![i32]);
        if crate::repr::optional_inner(ctx, left.get_type(ctx)).is_some() {
            let a_absent = self.is_absent_tag(ctx, at);
            let b_absent = self.is_absent_tag(ctx, bt);
            let either_absent = self.num(ctx, NumOpcode::I32Or, vec![a_absent, b_absent]);
            let absent_block = self.new_block(ctx, vec![]);
            let present_block = self.new_block(ctx, vec![]);
            self.cond_br(
                ctx,
                either_absent,
                (absent_block, vec![]),
                (present_block, vec![]),
            );
            self.block = absent_block;
            // Omission compares as `undefined`; ordering an absent operand is always false.
            let undef_tag = self.i32_const(ctx, UNDEFINED);
            let omit_tag = self.i32_const(ctx, OMITTED);
            let a_omit = self.num(ctx, NumOpcode::I32Eq, vec![at, omit_tag]);
            let b_omit = self.num(ctx, NumOpcode::I32Eq, vec![bt, omit_tag]);
            let norm_a = self.select(ctx, a_omit, undef_tag, at);
            let norm_b = self.select(ctx, b_omit, undef_tag, bt);
            let both_absent = self.num(ctx, NumOpcode::I32And, vec![a_absent, b_absent]);
            let same_absent = self.num(ctx, NumOpcode::I32Eq, vec![norm_a, norm_b]);
            let eq = self.num(ctx, NumOpcode::I32And, vec![both_absent, same_absent]);
            let res = match kind {
                CompareKind::Eq => eq,
                CompareKind::Ne => self.num(ctx, NumOpcode::I32Eqz, vec![eq]),
                CompareKind::Lt | CompareKind::Le | CompareKind::Gt | CompareKind::Ge => {
                    self.i32_const(ctx, 0)
                }
            };
            self.br(ctx, done, vec![res]);
            self.block = present_block;
        }
        let order = self.numeric_order(ctx, (at, ap), (bt, bp));
        let (opcode, rhs) = match kind {
            CompareKind::Eq => (NumOpcode::I32Eq, 0),
            CompareKind::Ne => (NumOpcode::I32Ne, 0),
            CompareKind::Lt => (NumOpcode::I32LtS, 0),
            CompareKind::Le => (NumOpcode::I32LeS, 0),
            CompareKind::Gt => (NumOpcode::I32Eq, 1),
            CompareKind::Ge => (NumOpcode::I32LeU, 1),
        };
        let rhs = self.i32_const(ctx, rhs);
        let result = self.num(ctx, opcode, vec![order, rhs]);
        self.br(ctx, done, vec![result]);
        self.block = done;
        Ok(done.deref(ctx).get_argument(0))
    }

    /// Exact partial order of present alternatives: -1, 0, 1, or 2 for unordered.
    fn numeric_order(
        &mut self,
        ctx: &mut Context,
        (at, ap): (Value, Value),
        (bt, bp): (Value, Value),
    ) -> Value {
        let i32 = i32_type(ctx);
        let done = self.new_block(ctx, vec![i32]);
        let floats = self.new_block(ctx, vec![]);
        let integers = self.new_block(ctx, vec![]);
        let af = self.num(ctx, NumOpcode::I32Eqz, vec![at]);
        let bf = self.num(ctx, NumOpcode::I32Eqz, vec![bt]);
        let either = self.num(ctx, NumOpcode::I32Or, vec![af, bf]);
        self.cond_br(ctx, either, (floats, vec![]), (integers, vec![]));
        self.block = integers;
        let an = self.numeric_negative(ctx, at, ap);
        let bn = self.numeric_negative(ctx, bt, bp);
        let less = self.num(ctx, NumOpcode::I64LtU, vec![ap, bp]);
        let greater = self.num(ctx, NumOpcode::I64GtU, vec![ap, bp]);
        let same_sign = self.num(ctx, NumOpcode::I32Sub, vec![greater, less]);
        let different_sign = self.num(ctx, NumOpcode::I32Xor, vec![an, bn]);
        let sign_order = self.num(ctx, NumOpcode::I32Sub, vec![bn, an]);
        let order = self.select(ctx, different_sign, sign_order, same_sign);
        self.br(ctx, done, vec![order]);
        self.block = floats;
        let both = self.num(ctx, NumOpcode::I32And, vec![af, bf]);
        let float_pair = self.new_block(ctx, vec![]);
        let mixed = self.new_block(ctx, vec![]);
        self.cond_br(ctx, both, (float_pair, vec![]), (mixed, vec![]));
        self.block = float_pair;
        let a = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![ap]);
        let b = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bp]);
        let less = self.num(ctx, NumOpcode::F64Lt, vec![a, b]);
        let greater = self.num(ctx, NumOpcode::F64Gt, vec![a, b]);
        let order = self.num(ctx, NumOpcode::I32Sub, vec![greater, less]);
        let a_nan = self.num(ctx, NumOpcode::F64Ne, vec![a, a]);
        let b_nan = self.num(ctx, NumOpcode::F64Ne, vec![b, b]);
        let nan = self.num(ctx, NumOpcode::I32Or, vec![a_nan, b_nan]);
        let unordered = self.i32_const(ctx, 2);
        let order = self.select(ctx, nan, unordered, order);
        self.br(ctx, done, vec![order]);
        self.block = mixed;
        let integer = self.select(ctx, af, bp, ap);
        let tag = self.select(ctx, af, bt, at);
        let bits = self.select(ctx, af, ap, bp);
        let float = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits]);
        let negative = self.numeric_negative(ctx, tag, integer);
        let order = self.integer_float_order(ctx, integer, negative, float);
        let order = self.apply_integer_sign(ctx, order, af);
        let nan = self.num(ctx, NumOpcode::F64Ne, vec![float, float]);
        let unordered = self.i32_const(ctx, 2);
        let order = self.select(ctx, nan, unordered, order);
        self.br(ctx, done, vec![order]);
        self.block = done;
        done.deref(ctx).get_argument(0)
    }

    fn apply_integer_sign(&mut self, ctx: &mut Context, value: Value, negative: Value) -> Value {
        let wide = integer(ctx, value.get_type(ctx)).is_some_and(|(bits, _)| bits == 64);
        let opcode = if wide {
            NumOpcode::I64Sub
        } else {
            NumOpcode::I32Sub
        };
        let zero = self.zero(ctx, value.get_type(ctx));
        let inverted = self.num(ctx, opcode, vec![zero, value]);
        self.select(ctx, negative, inverted, value)
    }

    fn numeric_negative(&mut self, ctx: &mut Context, tag: Value, payload: Value) -> Value {
        let one = self.i32_const(ctx, 1);
        let signed = self.num(ctx, NumOpcode::I32And, vec![tag, one]);
        let zero = self.i64_const(ctx, 0);
        let negative = self.num(ctx, NumOpcode::I64LtS, vec![payload, zero]);
        self.num(ctx, NumOpcode::I32And, vec![signed, negative])
    }

    /// Exact magnitude comparison between a 64-bit integer and an IEEE 754 `f64`.
    /// Truncation is exact in range, the equal-integer case retains any fractional remainder, and `2^64` bounds saturation.
    fn integer_float_order(
        &mut self,
        ctx: &mut Context,
        integer: Value,
        negative: Value,
        float: Value,
    ) -> Value {
        let magnitude = self.apply_integer_sign(ctx, integer, negative);
        let absolute = self.num(ctx, NumOpcode::F64Abs, vec![float]);
        let truncated = self.num(ctx, NumOpcode::I64TruncSatF64U, vec![absolute]);
        let rounded = self.num(ctx, NumOpcode::F64ConvertI64U, vec![truncated]);
        let fraction = self.num(ctx, NumOpcode::F64Lt, vec![rounded, absolute]);
        let equal = self.num(ctx, NumOpcode::I64Eq, vec![magnitude, truncated]);
        let less = self.num(ctx, NumOpcode::I64LtU, vec![magnitude, truncated]);
        let minus = self.i32_const(ctx, -1);
        let zero = self.i32_const(ctx, 0);
        let plus = self.i32_const(ctx, 1);
        let fraction_order = self.select(ctx, fraction, minus, zero);
        let order = self.select(ctx, less, minus, plus);
        let order = self.select(ctx, equal, fraction_order, order);
        let limit = self.f64_const(ctx, 18446744073709551616.0);
        let above = self.num(ctx, NumOpcode::F64Ge, vec![absolute, limit]);
        let order = self.select(ctx, above, minus, order);
        let order = self.apply_integer_sign(ctx, order, negative);
        let zero_float = self.f64_const(ctx, 0.0);
        let float_negative = self.num(ctx, NumOpcode::F64Lt, vec![float, zero_float]);
        let different_sign = self.num(ctx, NumOpcode::I32Xor, vec![negative, float_negative]);
        let sign_order = self.select(ctx, negative, minus, plus);
        self.select(ctx, different_sign, sign_order, order)
    }
}
