//! `Math` intrinsics over tagged numeric alternatives: integer tags stay exact.
use crate::{function::FunctionLowering, repr::Carriers};
use anyhow::Result;
use perry_abi::layout::numeric::{FLOAT, SIGNED, UNSIGNED};
use perry_mir::{CompareKind, Intrinsic, NumericType};
use pliron::{
    context::{Context, Ptr},
    operation::Operation,
    r#type::Typed,
    value::Value,
};
use pliron_wasm::attrs::NumOpcode;

impl FunctionLowering<'_, '_> {
    /// Lower a `Math` intrinsic with a numeric result over tagged operands; `None` when
    /// `intrinsic` has no tagged lowering.
    pub(crate) fn numeric_intrinsic(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        intrinsic: Intrinsic,
    ) -> Result<Option<Carriers<Value>>> {
        if op.deref(ctx).get_num_results() == 0 {
            return Ok(None);
        }
        let ty = op.deref(ctx).get_result(0).get_type(ctx);
        if !ty.deref(ctx).is::<NumericType>() {
            return Ok(None);
        }
        let (tag, payload) = match intrinsic {
            Intrinsic::MathFloor => {
                self.numeric_round(ctx, op, Rounding::Native(NumOpcode::F64Floor))?
            }
            Intrinsic::MathCeil => {
                self.numeric_round(ctx, op, Rounding::Native(NumOpcode::F64Ceil))?
            }
            Intrinsic::MathTrunc => {
                self.numeric_round(ctx, op, Rounding::Native(NumOpcode::F64Trunc))?
            }
            Intrinsic::MathRound => self.numeric_round(ctx, op, Rounding::HalfUp)?,
            Intrinsic::MathAbs => self.numeric_abs(ctx, op)?,
            Intrinsic::MathMin => self.numeric_extremum(ctx, op, true)?,
            Intrinsic::MathMax => self.numeric_extremum(ctx, op, false)?,
            _ => return Ok(None),
        };
        Ok(Some(Carriers::Numeric { tag, payload }))
    }

    /// ECMAScript rounds halves toward positive infinity while preserving the sign of zero.
    pub(crate) fn round_half_up(&mut self, ctx: &mut Context, value: Value) -> Value {
        let floor = self.num(ctx, NumOpcode::F64Floor, vec![value]);
        let fraction = self.num(ctx, NumOpcode::F64Sub, vec![value, floor]);
        let half = self.f64_const(ctx, 0.5);
        let up = self.num(ctx, NumOpcode::F64Ge, vec![fraction, half]);
        let one = self.f64_const(ctx, 1.0);
        let next = self.num(ctx, NumOpcode::F64Add, vec![floor, one]);
        let rounded = self.select(ctx, up, next, floor);
        self.num(ctx, NumOpcode::F64Copysign, vec![rounded, value])
    }

    /// Round a float operand; integer operands are already integral and pass through.
    fn numeric_round(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        rounding: Rounding,
    ) -> Result<(Value, Value)> {
        let arg = op.deref(ctx).get_operand(0);
        let (tag, payload) = self.mapped_value(ctx, arg)?.numeric()?;
        let f = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![payload]);
        let rounded = match rounding {
            Rounding::Native(opcode) => self.num(ctx, opcode, vec![f]),
            Rounding::HalfUp => self.round_half_up(ctx, f),
        };
        let float_payload = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![rounded]);
        let is_float = self.num(ctx, NumOpcode::I32Eqz, vec![tag]);
        let payload = self.select(ctx, is_float, float_payload, payload);
        Ok((tag, payload))
    }

    /// `Math.abs`: negating `i64::MIN` yields an unsigned magnitude.
    fn numeric_abs(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<(Value, Value)> {
        let arg = op.deref(ctx).get_operand(0);
        let (tag, payload) = self.mapped_value(ctx, arg)?.numeric()?;
        let f = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![payload]);
        let abs_f = self.num(ctx, NumOpcode::F64Abs, vec![f]);
        let float_payload = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![abs_f]);
        let signed_tag = self.i32_const(ctx, SIGNED);
        let unsigned_tag = self.i32_const(ctx, UNSIGNED);
        let one = self.i32_const(ctx, 1);
        let is_signed = self.num(ctx, NumOpcode::I32And, vec![tag, one]);
        let zero = self.i64_const(ctx, 0);
        let is_neg = self.num(ctx, NumOpcode::I64LtS, vec![payload, zero]);
        let negate = self.num(ctx, NumOpcode::I32And, vec![is_signed, is_neg]);
        let neg_payload = self.num(ctx, NumOpcode::I64Sub, vec![zero, payload]);
        let int_payload = self.select(ctx, negate, neg_payload, payload);
        let overflow_min = self.num(ctx, NumOpcode::I64LtS, vec![int_payload, zero]);
        let signed_res_tag = self.select(ctx, overflow_min, unsigned_tag, signed_tag);
        let int_tag = self.select(ctx, is_signed, signed_res_tag, tag);
        let is_float = self.num(ctx, NumOpcode::I32Eqz, vec![tag]);
        let res_tag = self.select(ctx, is_float, tag, int_tag);
        let res_payload = self.select(ctx, is_float, float_payload, int_payload);
        Ok((res_tag, res_payload))
    }

    /// `Math.min` (`min`) or `Math.max` of two operands; NaN wins either way.
    fn numeric_extremum(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        min: bool,
    ) -> Result<(Value, Value)> {
        let lhs = op.deref(ctx).get_operand(0);
        let rhs = op.deref(ctx).get_operand(1);
        let (lt, lp) = self.mapped_value(ctx, lhs)?.numeric()?;
        let (rt, rp) = self.mapped_value(ctx, rhs)?.numeric()?;
        let float_tag = self.i32_const(ctx, FLOAT);
        let lf = self.num(ctx, NumOpcode::I32Eq, vec![lt, float_tag]);
        let rf = self.num(ctx, NumOpcode::I32Eq, vec![rt, float_tag]);
        let lf_val = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![lp]);
        let rf_val = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![rp]);
        let l_nan_f = self.num(ctx, NumOpcode::F64Ne, vec![lf_val, lf_val]);
        let r_nan_f = self.num(ctx, NumOpcode::F64Ne, vec![rf_val, rf_val]);
        let l_nan = self.num(ctx, NumOpcode::I32And, vec![lf, l_nan_f]);
        let r_nan = self.num(ctx, NumOpcode::I32And, vec![rf, r_nan_f]);
        let any_nan = self.num(ctx, NumOpcode::I32Or, vec![l_nan, r_nan]);
        let is_less = self.numeric_comparison(ctx, op, CompareKind::Lt)?;
        let pick_left = if min {
            is_less
        } else {
            self.num(ctx, NumOpcode::I32Eqz, vec![is_less])
        };
        let chosen_tag = self.select(ctx, pick_left, lt, rt);
        let chosen_payload = self.select(ctx, pick_left, lp, rp);
        let nan_bits = self.i64_const(ctx, f64::NAN.to_bits());
        let res_tag = self.select(ctx, any_nan, float_tag, chosen_tag);
        let res_payload = self.select(ctx, any_nan, nan_bits, chosen_payload);
        Ok((res_tag, res_payload))
    }
}

/// How a `Math` rounding intrinsic rounds a float payload.
#[derive(Clone, Copy)]
enum Rounding {
    /// A single wasm rounding instruction (`floor`, `ceil`, `trunc`).
    Native(NumOpcode),
    /// `Math.round`: halves round toward +∞, keeping the sign of zero.
    HalfUp,
}
