//! Lower numeric conversions, semantic widths and lossless tagged alternatives.
use crate::{
    function::FunctionLowering,
    repr::{Carriers, i32_type, wasm_type},
    runtime::Runtime,
};
use anyhow::{Result, bail};
use perry_abi::layout::numeric::{FLOAT, SIGNED, UNSIGNED};
use perry_mir::{
    attrs::{BinaryKind, CompareKind, NumericConstant},
    types::NumericType,
};
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        ops::ConstantOp,
        types::{FP64Type, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    op::Op,
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};
use pliron_wasm::control::UnreachableOp;
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

    pub(super) fn cast_value(
        &mut self,
        ctx: &mut Context,
        value: Carriers<Value>,
        source: TypeHandle,
        target: TypeHandle,
    ) -> Result<Carriers<Value>> {
        if source == target {
            return Ok(value);
        }
        if target.deref(ctx).is::<NumericType>() {
            let value = value.scalar()?;
            let (tag, payload) = if let Some((_, signed)) = integer(ctx, source) {
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
                    if signed { SIGNED } else { UNSIGNED },
                    self.numeric_cast(ctx, value, source, wide)?,
                )
            } else {
                (
                    FLOAT,
                    self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value]),
                )
            };
            return Ok(Carriers::Numeric {
                tag: self.i32_const(ctx, tag),
                payload,
            });
        }
        let value = match value {
            Carriers::Scalar(value) => self.numeric_cast(ctx, value, source, target)?,
            Carriers::Numeric { tag, payload } => {
                let result_type = wasm_type(ctx, target)?;
                let done = self.new_block(ctx, vec![result_type]);
                let floats = self.new_block(ctx, vec![]);
                let signed = self.new_block(ctx, vec![]);
                let unsigned = if target.deref(ctx).is::<FP64Type>() {
                    self.new_block(ctx, vec![])
                } else {
                    signed
                };
                let invalid = self.new_block(ctx, vec![]);
                self.br_table(ctx, tag, vec![floats, signed, unsigned], invalid);
                self.block = invalid;
                let trap = UnreachableOp::new(ctx).get_operation();
                self.insert(ctx, trap);
                self.block = floats;
                let float = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![payload]);
                let float_type = FP64Type::get(ctx).into();
                let float = self.numeric_cast(ctx, float, float_type, target)?;
                self.br(ctx, done, vec![float]);
                for (block, signedness) in [
                    (signed, Signedness::Signed),
                    (unsigned, Signedness::Unsigned),
                ] {
                    if block == signed && signedness == Signedness::Unsigned {
                        continue;
                    }
                    self.block = block;
                    let source = IntegerType::get(ctx, 64, signedness).into();
                    let value = self.numeric_cast(ctx, payload, source, target)?;
                    self.br(ctx, done, vec![value]);
                }
                self.block = done;
                done.deref(ctx).get_argument(0)
            }
        };
        Ok(Carriers::Scalar(value))
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
        let (
            Carriers::Numeric {
                tag: lt,
                payload: lp,
            },
            Carriers::Numeric {
                tag: rt,
                payload: rp,
            },
        ) = (left, right)
        else {
            bail!("numeric arithmetic requires tagged operands");
        };
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
            let unsigned = self.i32_const(ctx, UNSIGNED);
            let lu = self.num(ctx, NumOpcode::I32Eq, vec![lt, unsigned]);
            let ru = self.num(ctx, NumOpcode::I32Eq, vec![rt, unsigned]);
            let is_unsigned = self.num(ctx, NumOpcode::I32Or, vec![lu, ru]);
            let signed = self.i32_const(ctx, SIGNED);
            let tag = self.select(ctx, is_unsigned, unsigned, signed);
            if kind == BinaryKind::Rem {
                let zero = self.num(ctx, NumOpcode::I64Eqz, vec![rp]);
                let nan = self.new_block(ctx, vec![]);
                let nonzero = self.new_block(ctx, vec![]);
                self.cond_br(ctx, zero, (nan, vec![]), (nonzero, vec![]));
                self.block = nan;
                let nan = self.numeric_constant(ctx, NumericConstant::Float(f64::NAN.to_bits()));
                self.br(ctx, done, nan.iter().collect());
                self.block = nonzero;
                let unsigned_block = self.new_block(ctx, vec![]);
                let signed_block = self.new_block(ctx, vec![]);
                self.cond_br(
                    ctx,
                    is_unsigned,
                    (unsigned_block, vec![]),
                    (signed_block, vec![]),
                );
                self.block = unsigned_block;
                let value = self.num(ctx, NumOpcode::I64RemU, vec![lp, rp]);
                self.br(ctx, done, vec![unsigned, value]);
                self.block = signed_block;
                let value = self.num(ctx, NumOpcode::I64RemS, vec![lp, rp]);
                let empty = self.num(ctx, NumOpcode::I64Eqz, vec![value]);
                let zero = self.i64_const(ctx, 0);
                let negative = self.num(ctx, NumOpcode::I64LtS, vec![lp, zero]);
                let negative_zero = self.num(ctx, NumOpcode::I32And, vec![empty, negative]);
                let float = self.i32_const(ctx, FLOAT);
                let bits = self.i64_const(ctx, (-0.0f64).to_bits());
                let tag = self.select(ctx, negative_zero, float, signed);
                let value = self.select(ctx, negative_zero, bits, value);
                self.br(ctx, done, vec![tag, value]);
            } else {
                let integer_type = IntegerType::get(ctx, 64, Signedness::Unsigned).into();
                let value = self.scalar_binary(ctx, kind, integer_type, vec![lp, rp], None)?;
                let (tag, value) = if kind == BinaryKind::Mul {
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
                        self.select(ctx, negative_zero, bits, value),
                    )
                } else {
                    (tag, value)
                };
                self.br(ctx, done, vec![tag, value]);
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
        if let Some((bits, _)) = integer(ctx, ty) {
            let opcode = match (kind, bits == 64) {
                (BinaryKind::Add, false) => NumOpcode::I32Add,
                (BinaryKind::Sub, false) => NumOpcode::I32Sub,
                (BinaryKind::Mul, false) => NumOpcode::I32Mul,
                (BinaryKind::Add, true) => NumOpcode::I64Add,
                (BinaryKind::Sub, true) => NumOpcode::I64Sub,
                (BinaryKind::Mul, true) => NumOpcode::I64Mul,
                _ => bail!("integer {kind:?} requires explicit division semantics"),
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

    pub(super) fn scalar_comparison(
        &mut self,
        ctx: &mut Context,
        kind: CompareKind,
        ty: TypeHandle,
        left: Value,
        right: Value,
    ) -> Value {
        if left.get_type(ctx).deref(ctx).is::<FP64Type>() {
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
            let (bits, signed) = integer(ctx, ty).unwrap_or((32, true));
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
        }
    }

    pub(super) fn numeric_comparison(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        kind: CompareKind,
    ) -> Result<Value> {
        let (left, right) = (op.deref(ctx).get_operand(0), op.deref(ctx).get_operand(1));
        let (a, b) = (
            self.mapped_value(ctx, left)?,
            self.mapped_value(ctx, right)?,
        );
        let (
            Carriers::Numeric {
                tag: at,
                payload: ap,
            },
            Carriers::Numeric {
                tag: bt,
                payload: bp,
            },
        ) = (a, b)
        else {
            bail!("numeric comparison requires tagged operands");
        };
        let i32 = i32_type(ctx);
        let done = self.new_block(ctx, vec![i32]);
        let floats = self.new_block(ctx, vec![]);
        let integers = self.new_block(ctx, vec![]);
        let float = self.i32_const(ctx, FLOAT);
        let af = self.num(ctx, NumOpcode::I32Eq, vec![at, float]);
        let bf = self.num(ctx, NumOpcode::I32Eq, vec![bt, float]);
        let either = self.num(ctx, NumOpcode::I32Or, vec![af, bf]);
        self.cond_br(ctx, either, (floats, vec![]), (integers, vec![]));
        self.block = integers;
        let an = self.numeric_negative(ctx, at, ap);
        let bn = self.numeric_negative(ctx, bt, bp);
        let integer = IntegerType::get(ctx, 64, Signedness::Unsigned).into();
        let same_sign = self.scalar_comparison(ctx, kind, integer, ap, bp);
        let different_sign = self.num(ctx, NumOpcode::I32Xor, vec![an, bn]);
        let sign_result = match kind {
            CompareKind::Eq => self.i32_const(ctx, 0),
            CompareKind::Ne => self.i32_const(ctx, 1),
            CompareKind::Lt | CompareKind::Le => an,
            CompareKind::Gt | CompareKind::Ge => bn,
        };
        let result = self.select(ctx, different_sign, sign_result, same_sign);
        self.br(ctx, done, vec![result]);
        self.block = floats;
        let both = self.num(ctx, NumOpcode::I32And, vec![af, bf]);
        let float_pair = self.new_block(ctx, vec![]);
        let mixed = self.new_block(ctx, vec![]);
        self.cond_br(ctx, both, (float_pair, vec![]), (mixed, vec![]));
        self.block = float_pair;
        let a = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![ap]);
        let b = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bp]);
        let float_type = FP64Type::get(ctx).into();
        let result = self.scalar_comparison(ctx, kind, float_type, a, b);
        self.br(ctx, done, vec![result]);
        self.block = mixed;
        let integer = self.select(ctx, af, bp, ap);
        let tag = self.select(ctx, af, bt, at);
        let bits = self.select(ctx, af, ap, bp);
        let float = self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits]);
        let negative = self.numeric_negative(ctx, tag, integer);
        let order = self.integer_float_order(ctx, integer, negative, float);
        let zero = self.i32_const(ctx, 0);
        let reversed = self.num(ctx, NumOpcode::I32Sub, vec![zero, order]);
        let order = self.select(ctx, af, reversed, order);
        let nan = self.num(ctx, NumOpcode::F64Ne, vec![float, float]);
        let unordered = self.i32_const(ctx, 2);
        let order = self.select(ctx, nan, unordered, order);
        let plus = self.i32_const(ctx, 1);
        let (opcode, rhs) = match kind {
            CompareKind::Eq => (NumOpcode::I32Eq, zero),
            CompareKind::Ne => (NumOpcode::I32Ne, zero),
            CompareKind::Lt => (NumOpcode::I32LtS, zero),
            CompareKind::Le => (NumOpcode::I32LeS, zero),
            CompareKind::Gt => (NumOpcode::I32Eq, plus),
            CompareKind::Ge => (NumOpcode::I32LeU, plus),
        };
        let result = self.num(ctx, opcode, vec![order, rhs]);
        self.br(ctx, done, vec![result]);
        self.block = done;
        Ok(done.deref(ctx).get_argument(0))
    }

    fn numeric_negative(&mut self, ctx: &mut Context, tag: Value, payload: Value) -> Value {
        let signed = self.i32_const(ctx, SIGNED);
        let signed = self.num(ctx, NumOpcode::I32Eq, vec![tag, signed]);
        let zero = self.i64_const(ctx, 0);
        let negative = self.num(ctx, NumOpcode::I64LtS, vec![payload, zero]);
        self.num(ctx, NumOpcode::I32And, vec![signed, negative])
    }

    /// Compare magnitudes without rounding the integer. Truncation is exact in range;
    /// the equal-integer case retains a fractional remainder, and 2^64 bounds saturation.
    fn integer_float_order(
        &mut self,
        ctx: &mut Context,
        integer: Value,
        negative: Value,
        float: Value,
    ) -> Value {
        let zero = self.i64_const(ctx, 0);
        let negated = self.num(ctx, NumOpcode::I64Sub, vec![zero, integer]);
        let magnitude = self.select(ctx, negative, negated, integer);
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
        let negated = self.num(ctx, NumOpcode::I32Sub, vec![zero, order]);
        let order = self.select(ctx, negative, negated, order);
        let zero_float = self.f64_const(ctx, 0.0);
        let float_negative = self.num(ctx, NumOpcode::F64Lt, vec![float, zero_float]);
        let different_sign = self.num(ctx, NumOpcode::I32Xor, vec![negative, float_negative]);
        let sign_order = self.select(ctx, negative, minus, plus);
        self.select(ctx, different_sign, sign_order, order)
    }
}
