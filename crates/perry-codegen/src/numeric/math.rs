//! Scalar and tagged Math share operation selection and integer interpretation.
use super::integer;
use crate::{function::FunctionLowering, repr::Carriers};
use anyhow::Result;
use perry_abi::layout::numeric::{FLOAT, SIGNED, UNSIGNED};
use perry_mir::{CompareKind, Intrinsic};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_wasm::attrs::NumOpcode;

impl FunctionLowering<'_, '_> {
    pub(crate) fn bitwise_intrinsic(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        intrinsic: Intrinsic,
    ) -> Result<Option<Carriers<Value>>> {
        let opcode = match intrinsic {
            Intrinsic::MathImul => NumOpcode::I32Mul,
            Intrinsic::BitAnd => NumOpcode::I32And,
            Intrinsic::BitOr => NumOpcode::I32Or,
            Intrinsic::BitXor | Intrinsic::BitNot => NumOpcode::I32Xor,
            Intrinsic::ShiftLeft => NumOpcode::I32Shl,
            Intrinsic::ShiftRight => NumOpcode::I32ShrS,
            Intrinsic::ShiftRightUnsigned => NumOpcode::I32ShrU,
            _ => return Ok(None),
        };
        let signedness = if intrinsic == Intrinsic::ShiftRightUnsigned {
            Signedness::Unsigned
        } else {
            Signedness::Signed
        };
        let integer_ty: TypeHandle = IntegerType::get(ctx, 32, signedness).into();
        let mut args = Vec::with_capacity(2);
        let count = op.deref(ctx).get_num_operands();
        for index in 0..count {
            let operand = op.deref(ctx).get_operand(index);
            let source = operand.get_type(ctx);
            let value = self.operand(ctx, op, index)?;
            let bits = if source.deref(ctx).is::<FP64Type>() {
                let bounds = self.state.program_facts.integral_interval(ctx, operand);
                self.int32_bits(ctx, value, signedness == Signedness::Signed, bounds)
            } else {
                self.numeric_cast(ctx, value, source, integer_ty)?
            };
            args.push(bits);
        }
        if intrinsic == Intrinsic::BitNot {
            args.push(self.i32_const(ctx, -1));
        }
        // Wasm shifts mask their count to five bits, as JavaScript requires.
        let bits = self.num(ctx, opcode, args);
        let result_ty = op.deref(ctx).get_result(0).get_type(ctx);
        Ok(Some(self.cast_value(
            ctx,
            Carriers::Scalar(bits),
            integer_ty,
            result_ty,
        )?))
    }

    pub(crate) fn numeric_intrinsic(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        intrinsic: Intrinsic,
    ) -> Result<Option<Carriers<Value>>> {
        let opcode = match intrinsic {
            Intrinsic::MathFloor => Some(NumOpcode::F64Floor),
            Intrinsic::MathCeil => Some(NumOpcode::F64Ceil),
            Intrinsic::MathTrunc => Some(NumOpcode::F64Trunc),
            Intrinsic::MathAbs => Some(NumOpcode::F64Abs),
            Intrinsic::MathMin => Some(NumOpcode::F64Min),
            Intrinsic::MathMax => Some(NumOpcode::F64Max),
            Intrinsic::MathRound => None,
            _ => return Ok(None),
        };
        let ty = op.deref(ctx).get_result(0).get_type(ctx);
        let lhs = op.deref(ctx).get_operand(0);
        let value = self.mapped_value(ctx, lhs)?;
        let extremum = matches!(intrinsic, Intrinsic::MathMin | Intrinsic::MathMax);
        let (float, tagged) = match value {
            Carriers::Scalar(value) => {
                if let Some((_, signed)) = integer(ctx, ty) {
                    let result = if intrinsic == Intrinsic::MathAbs && signed {
                        let zero = self.zero(ctx, value.get_type(ctx));
                        let negative =
                            self.scalar_comparison(ctx, CompareKind::Lt, ty, value, zero)?;
                        let magnitude = self.apply_integer_sign(ctx, value, negative);
                        self.normalize_integer(ctx, magnitude, ty)
                    } else if extremum {
                        let rhs = self.operand(ctx, op, 1)?;
                        let less = self.scalar_comparison(ctx, CompareKind::Lt, ty, value, rhs)?;
                        let (a, b) = if intrinsic == Intrinsic::MathMin {
                            (value, rhs)
                        } else {
                            (rhs, value)
                        };
                        self.select(ctx, less, a, b)
                    } else {
                        value
                    };
                    return Ok(Some(Carriers::Scalar(result)));
                }
                (value, None)
            }
            Carriers::Numeric { tag, payload } => {
                if extremum {
                    let rhs = op.deref(ctx).get_operand(1);
                    let rhs = self.mapped_value(ctx, rhs)?.numeric()?;
                    let (tag, payload) = self.numeric_extremum(
                        ctx,
                        (tag, payload),
                        rhs,
                        intrinsic == Intrinsic::MathMin,
                    );
                    return Ok(Some(Carriers::Numeric { tag, payload }));
                }
                let float = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![payload]);
                (float, Some((tag, payload)))
            }
        };
        let float = match opcode {
            Some(opcode) => {
                let operands = if extremum {
                    vec![float, self.operand(ctx, op, 1)?]
                } else {
                    vec![float]
                };
                self.num(ctx, opcode, operands)
            }
            None => self.round_half_up(ctx, float),
        };
        let Some((tag, payload)) = tagged else {
            return Ok(Some(Carriers::Scalar(float)));
        };
        let (integer_tag, integer_payload) = if intrinsic == Intrinsic::MathAbs {
            let negative = self.numeric_negative(ctx, tag, payload);
            let magnitude = self.apply_integer_sign(ctx, payload, negative);
            let signed_tag = self.i32_const(ctx, SIGNED);
            let unsigned_tag = self.i32_const(ctx, UNSIGNED);
            let zero = self.i64_const(ctx, 0);
            let overflow = self.num(ctx, NumOpcode::I64LtS, vec![magnitude, zero]);
            let positive_tag = self.select(ctx, overflow, unsigned_tag, signed_tag);
            let one = self.i32_const(ctx, 1);
            let signed = self.num(ctx, NumOpcode::I32And, vec![tag, one]);
            (self.select(ctx, signed, positive_tag, tag), magnitude)
        } else {
            (tag, payload)
        };
        let float_bits = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![float]);
        let is_float = self.num(ctx, NumOpcode::I32Eqz, vec![tag]);
        Ok(Some(Carriers::Numeric {
            tag: self.select(ctx, is_float, tag, integer_tag),
            payload: self.select(ctx, is_float, float_bits, integer_payload),
        }))
    }

    /// ECMAScript rounds halves toward positive infinity while preserving the sign of zero.
    fn round_half_up(&mut self, ctx: &mut Context, value: Value) -> Value {
        let floor = self.num(ctx, NumOpcode::F64Floor, vec![value]);
        let fraction = self.num(ctx, NumOpcode::F64Sub, vec![value, floor]);
        let half = self.f64_const(ctx, 0.5);
        let up = self.num(ctx, NumOpcode::F64Ge, vec![fraction, half]);
        let one = self.f64_const(ctx, 1.0);
        let next = self.num(ctx, NumOpcode::F64Add, vec![floor, one]);
        let rounded = self.select(ctx, up, next, floor);
        self.num(ctx, NumOpcode::F64Copysign, vec![rounded, value])
    }

    /// Compute `Math.min` (`min`) or `Math.max` of two operands, propagating `NaN` from either operand.
    fn numeric_extremum(
        &mut self,
        ctx: &mut Context,
        (lt, lp): (Value, Value),
        (rt, rp): (Value, Value),
        min: bool,
    ) -> (Value, Value) {
        let order = self.numeric_order(ctx, (lt, lp), (rt, rp));
        let zero = self.i32_const(ctx, 0);
        let two = self.i32_const(ctx, 2);
        let any_nan = self.num(ctx, NumOpcode::I32Eq, vec![order, two]);
        let ordered = self.num(
            ctx,
            if min {
                NumOpcode::I32LtS
            } else {
                NumOpcode::I32GtS
            },
            vec![order, zero],
        );
        // Equal values choose the negative zero for min and positive zero for max.
        let equal = self.num(ctx, NumOpcode::I32Eqz, vec![order]);
        let negative_zero = self.i64_const(ctx, (-0.0f64).to_bits());
        let left_negative_zero = self.num(ctx, NumOpcode::I64Eq, vec![lp, negative_zero]);
        let left_float = self.num(ctx, NumOpcode::I32Eqz, vec![lt]);
        let left_negative_zero =
            self.num(ctx, NumOpcode::I32And, vec![left_negative_zero, left_float]);
        let tie_left = if min {
            left_negative_zero
        } else {
            self.num(ctx, NumOpcode::I32Eqz, vec![left_negative_zero])
        };
        let pick_left = self.select(ctx, equal, tie_left, ordered);
        let float_tag = self.i32_const(ctx, FLOAT);
        let chosen_tag = self.select(ctx, pick_left, lt, rt);
        let chosen_payload = self.select(ctx, pick_left, lp, rp);
        let nan_bits = self.i64_const(ctx, f64::NAN.to_bits());
        let res_tag = self.select(ctx, any_nan, float_tag, chosen_tag);
        let res_payload = self.select(ctx, any_nan, nan_bits, chosen_payload);
        (res_tag, res_payload)
    }
}
