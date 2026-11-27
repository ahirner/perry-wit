//! Lower numeric conversions, semantic widths and lossless tagged alternatives.
mod math;

use crate::{
    function::FunctionLowering,
    repr::{Carriers, Repr, repr},
    runtime::Runtime,
    values::is_numeric_type,
};
use perry_abi::layout::numeric::{FLOAT, Integer, NULL, OMITTED, SIGNED, UNDEFINED, UNSIGNED};
use perry_mir::{BinaryKind, CompareKind, NumericConstant, NumericType};
use perry_wit_diagnostics::{Diagnostic, Result, bail_diag, bail_internal, codes, ensure_internal};
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

impl FunctionLowering<'_, '_> {
    /// ECMAScript `ToInt32` of an `f64`, as the `i32` bits `ToUint32`, `ToInt16` and the other
    /// narrower conversions share: truncate, map `NaN` and infinities to 0, wrap modulo 2^32.
    /// `bounds`, an interval `ProgramFacts` proved for `x`, selects a cheaper exact sequence:
    /// one saturating truncation inside the 32-bit range of `signed`, or a 64-bit truncation
    /// and wrap within ±2^62, where rounding a large `f64` cannot reach the `i64` limits.
    /// Only an unbounded `x` needs the reduction
    /// `x - trunc(x / 2^32) * 2^32`, which is exact and lies within (-2^32, 2^32).
    pub(crate) fn int32_bits(
        &mut self,
        ctx: &mut Context,
        x: Value,
        signed: bool,
        bounds: Option<(i128, i128)>,
    ) -> Value {
        let range = if signed {
            (i128::from(i32::MIN), i128::from(i32::MAX))
        } else {
            (0, i128::from(u32::MAX))
        };
        let x = match bounds {
            Some((lower, upper)) if lower >= range.0 && upper <= range.1 => {
                let truncate = if signed {
                    NumOpcode::I32TruncSatF64S
                } else {
                    NumOpcode::I32TruncSatF64U
                };
                return self.num(ctx, truncate, vec![x]);
            }
            Some((lower, upper)) if lower >= -(1 << 62) && upper <= 1 << 62 => x,
            _ => {
                let scale_down = self.f64_const(ctx, 1.0 / 4_294_967_296.0);
                let scale_up = self.f64_const(ctx, 4_294_967_296.0);
                let wraps = self.num(ctx, NumOpcode::F64Mul, vec![x, scale_down]);
                let wraps = self.num(ctx, NumOpcode::F64Trunc, vec![wraps]);
                let wrapped = self.num(ctx, NumOpcode::F64Mul, vec![wraps, scale_up]);
                self.num(ctx, NumOpcode::F64Sub, vec![x, wrapped])
            }
        };
        let integer = self.num(ctx, NumOpcode::I64TruncSatF64S, vec![x]);
        self.num(ctx, NumOpcode::I32WrapI64, vec![integer])
    }

    /// A number as the 32-bit or narrower integer `target`, converted like ECMAScript
    /// (`ToInt32` and its narrower forms): see [Self::int32_bits] for `bounds`.
    pub(crate) fn narrow_number(
        &mut self,
        ctx: &mut Context,
        x: Value,
        target: TypeHandle,
        signed: bool,
        bounds: Option<(i128, i128)>,
    ) -> Value {
        let bits = self.int32_bits(ctx, x, signed, bounds);
        self.normalize_integer(ctx, bits, target)
    }

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
                .ok_or_else(|| Diagnostic::internal("bigint boxing needs an integer"))?;
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
        // A number becomes a 32-bit or narrower integer as in ECMAScript (`ToInt32` and its
        // narrower forms), the conversion typed arrays and JavaScript component bindings use.
        if source.deref(ctx).is::<FP64Type>()
            && let Some((8 | 16 | 32, signed)) = integer(ctx, target)
        {
            return Ok(self.narrow_number(ctx, value, target, signed, None));
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
            _ => bail_internal!(
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
                Integer::new(bits, signed)
                    .ok_or_else(|| {
                        perry_wit_diagnostics::internal!("{bits}-bit integers have no numeric tag")
                    })?
                    .tag(),
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

    /// Whether `tag` is `undefined`, `null` or omitted.
    fn is_absent_tag(&mut self, ctx: &mut Context, tag: Value) -> Value {
        let undef_tag = self.i32_const(ctx, UNDEFINED);
        let omit_tag = self.i32_const(ctx, OMITTED);
        let ge_undef = self.num(ctx, NumOpcode::I32GeU, vec![tag, undef_tag]);
        let le_omit = self.num(ctx, NumOpcode::I32LeU, vec![tag, omit_tag]);
        self.num(ctx, NumOpcode::I32And, vec![ge_undef, le_omit])
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
        let (at, a) = self.numeric_operand(ctx, op, 0)?;
        let (bt, b) = self.numeric_operand(ctx, op, 1)?;
        let function = match kind {
            BinaryKind::Add => Runtime::NumberAdd,
            BinaryKind::Sub => Runtime::NumberSub,
            BinaryKind::Mul => Runtime::NumberMul,
            BinaryKind::Rem => Runtime::NumberRem,
            BinaryKind::Div => {
                let float = FP64Type::get(ctx).into();
                let a = self.untag_numeric(ctx, at, a, float)?;
                let b = self.untag_numeric(ctx, bt, b, float)?;
                let quotient = self.num(ctx, NumOpcode::F64Div, vec![a, b]);
                return self.cast_value(ctx, Carriers::Scalar(quotient), float, ty);
            }
        };
        let widen = Operation::get_op::<perry_mir::BinaryOp>(op, ctx)
            .is_some_and(|op| op.is_unconstrained(ctx));
        let widen = self.i32_const(ctx, i32::from(widen));
        Ok(self.tagged_call(ctx, function, vec![at, a, bt, b, widen]))
    }

    pub(super) fn tagged_call(
        &mut self,
        ctx: &mut Context,
        function: Runtime,
        mut args: Vec<Value>,
    ) -> Carriers<Value> {
        let slot = self.numeric_result_slot(ctx);
        args.push(slot);
        let tag = self.runtime(ctx, function, args);
        let payload = self.load(ctx, MemKind::I64, slot, 0);
        Carriers::Numeric { tag, payload }
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
                _ => bail_diag!(
                    codes::UNSUPPORTED_OPERATOR,
                    "bigint {kind:?} is not supported yet"
                ),
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
            bail_internal!("{kind:?} requires a resolved numeric representation");
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
        self.join_value(ctx, join, rem)
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
                    ensure_internal!(
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
        let relation = self.runtime(ctx, Runtime::NumberRelation, vec![at, ap, bt, bp]);
        let mask = self.i32_const(ctx, kind.accepted_relations());
        let selected = self.num(ctx, NumOpcode::I32And, vec![relation, mask]);
        let zero = self.i32_const(ctx, 0);
        Ok(self.num(ctx, NumOpcode::I32Ne, vec![selected, zero]))
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
}
