//! Operators: arithmetic, comparison, logical, unary and truthiness.
use super::*;
use crate::coerce::{expression_hint, merge_absence, merged_type};

/// The truthiness operators `&&` and `||`; `??` tests nullishness and lowers separately.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShortCircuit {
    And,
    Or,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Equality {
    SameValue,
    SameValueZero,
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn same_value(
        &mut self,
        ctx: &mut Context,
        lhs: Value,
        rhs: Value,
        equality: Equality,
    ) -> Result<Value> {
        let equal = match equality {
            Equality::SameValue => self.comparison_values(ctx, &CompareOp::Eq, lhs, rhs)?,
            Equality::SameValueZero => self.compare(ctx, CompareKind::Eq, lhs, rhs),
        };
        let numeric = |value: Value| Kind::of(ctx, present_type(ctx, value.get_type(ctx)));
        let numbers = numeric(lhs) == Some(Kind::Number) && numeric(rhs) == Some(Kind::Number);
        if !numbers
            && !(equality == Equality::SameValueZero && numeric(lhs) == Some(Kind::NumberString))
        {
            return Ok(equal);
        }
        let bool_ty = equal.get_type(ctx);
        self.choose(
            ctx,
            bool_ty,
            equal,
            |b, ctx| {
                if equality == Equality::SameValueZero {
                    return Ok(equal);
                }
                // Strict equality already established matching absence states. Coercion
                // makes both nulls +0 and both undefineds NaN without unwrapping either.
                let lhs = b.arithmetic(ctx, lhs);
                let rhs = b.arithmetic(ctx, rhs);
                let zero = b.number(ctx, 0.0);
                let is_zero = b.compare(ctx, CompareKind::Eq, lhs, zero);
                b.choose(
                    ctx,
                    bool_ty,
                    is_zero,
                    |b, ctx| {
                        let one = b.number(ctx, 1.0);
                        let lhs = b.binary(ctx, BinaryKind::Div, one, lhs);
                        let rhs = b.binary(ctx, BinaryKind::Div, one, rhs);
                        Ok(b.compare(ctx, CompareKind::Eq, lhs, rhs))
                    },
                    |_, _| Ok(equal),
                )
            },
            |b, ctx| {
                let lhs_nan = b.compare(ctx, CompareKind::Ne, lhs, lhs);
                b.choose(
                    ctx,
                    bool_ty,
                    lhs_nan,
                    |b, ctx| Ok(b.compare(ctx, CompareKind::Ne, rhs, rhs)),
                    |_, _| Ok(lhs_nan),
                )
            },
        )
    }

    /// `left op right` for arithmetic operators and string concatenation.
    pub(super) fn binary_expression(
        &mut self,
        ctx: &mut Context,
        op: &BinaryOp,
        left: &'s Expr,
        right: &'s Expr,
    ) -> Result<Value> {
        let lhs = self.expression(ctx, left)?;
        let rhs = self.expression(ctx, right)?;
        let string = |value: Value| {
            present_type(ctx, value.get_type(ctx))
                .deref(ctx)
                .is::<StringType>()
        };
        if matches!(op, BinaryOp::Add) && (string(lhs) || string(rhs)) {
            let lhs = self.display(ctx, lhs)?;
            let rhs = self.display(ctx, rhs)?;
            return Ok(self.emit_value(ctx, |ctx| ConcatOp::new(ctx, lhs, rhs)));
        }
        let lhs = self.arithmetic(ctx, lhs);
        let rhs = self.arithmetic(ctx, rhs);
        Ok(match (op, kind(ctx, lhs)?, kind(ctx, rhs)?) {
            (BinaryOp::Add, Kind::String, Kind::String) => {
                self.emit_value(ctx, |ctx| ConcatOp::new(ctx, lhs, rhs))
            }
            (BinaryOp::Add, Kind::String, _) | (BinaryOp::Add, _, Kind::String) => {
                let lhs = self.display(ctx, lhs)?;
                let rhs = self.display(ctx, rhs)?;
                self.emit_value(ctx, |ctx| ConcatOp::new(ctx, lhs, rhs))
            }
            (op, Kind::Number, Kind::Number) => {
                let kind = match op {
                    BinaryOp::Add => BinaryKind::Add,
                    BinaryOp::Sub => BinaryKind::Sub,
                    BinaryOp::Mul => BinaryKind::Mul,
                    BinaryOp::Div => BinaryKind::Div,
                    BinaryOp::Mod => BinaryKind::Rem,
                    other => {
                        return Err(PerryDiagnostic::error(
                            DiagnosticCode::UnsupportedOperator,
                            format!("MIR backend does not support operator {other:?} yet"),
                        )
                        .into());
                    }
                };
                self.binary(ctx, kind, lhs, rhs)
            }
            (
                op
                @ (BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod),
                Kind::BigInt,
                Kind::BigInt,
            ) => {
                let kind = match op {
                    BinaryOp::Add => BinaryKind::Add,
                    BinaryOp::Sub => BinaryKind::Sub,
                    BinaryOp::Mul => BinaryKind::Mul,
                    BinaryOp::Div => BinaryKind::Div,
                    _ => BinaryKind::Rem,
                };
                self.binary(ctx, kind, lhs, rhs)
            }
            (op, left_kind, right_kind) => {
                bail!(
                    "operator {op:?} on {} and {} is not supported yet",
                    left_kind.label(),
                    right_kind.label()
                )
            }
        })
    }

    /// `left op right` for strict equality and ordering; loose equality is rejected.
    pub(super) fn comparison(
        &mut self,
        ctx: &mut Context,
        op: &CompareOp,
        left: &'s Expr,
        right: &'s Expr,
    ) -> Result<Value> {
        let (equality, loose) = match op {
            CompareOp::Eq => (Some(CompareKind::Eq), false),
            CompareOp::Ne => (Some(CompareKind::Ne), false),
            CompareOp::LooseEq => (Some(CompareKind::Eq), true),
            CompareOp::LooseNe => (Some(CompareKind::Ne), true),
            _ => (None, false),
        };
        if let Some(equality) = equality
            && let Some(value) = self.absence_test(ctx, equality, loose, left, right)?
        {
            return Ok(value);
        }
        if loose {
            bail!("loose equality is not supported; use === or !==");
        }
        let lhs = self.expression(ctx, left)?;
        let rhs = self.expression(ctx, right)?;
        self.comparison_values(ctx, op, lhs, rhs)
    }

    pub(super) fn comparison_values(
        &mut self,
        ctx: &mut Context,
        op: &CompareOp,
        lhs: Value,
        rhs: Value,
    ) -> Result<Value> {
        let equality = match op {
            CompareOp::Eq => Some(CompareKind::Eq),
            CompareOp::Ne => Some(CompareKind::Ne),
            _ => None,
        };
        if let Some(equality) = equality {
            let exception = |value: Value| value.get_type(ctx).deref(ctx).is::<ExceptionType>();
            if exception(lhs) || exception(rhs) {
                let equal = if exception(lhs) && exception(rhs) {
                    self.intrinsic(ctx, Intrinsic::ExceptionEqual, vec![lhs, rhs])
                } else {
                    let (exception, value) = if exception(lhs) {
                        (lhs, rhs)
                    } else {
                        (rhs, lhs)
                    };
                    if value.get_type(ctx).deref(ctx).is::<perry_mir::BigIntType>() {
                        self.intrinsic(ctx, Intrinsic::ExceptionBigIntEqual, vec![exception, value])
                    } else {
                        let value = self.error_cause(ctx, value)?;
                        self.intrinsic(ctx, Intrinsic::JsonExceptionEqual, vec![value, exception])
                    }
                };
                return Ok(self.equality_result(ctx, equality, equal));
            }
            let json_left = self.json_operand(ctx, lhs)?;
            let json_right = self.json_operand(ctx, rhs)?;
            let operands = match (json_left, json_right) {
                (Some(left), Some(right)) => {
                    let equal = self.intrinsic(ctx, Intrinsic::JsonStrictEqual, vec![left, right]);
                    return Ok(self.equality_result(ctx, equality, equal));
                }
                (Some(json), None) => Some((json, rhs)),
                (None, Some(json)) => Some((json, lhs)),
                (None, None) => None,
            };
            if let Some((json, scalar)) = operands {
                let equal = self.json_scalar_equal(ctx, json, scalar)?;
                return Ok(self.equality_result(ctx, equality, equal));
            }
            let union = |value: Value| {
                present_type(ctx, value.get_type(ctx))
                    .deref(ctx)
                    .is::<perry_mir::NumberStringType>()
            };
            let states = merge_absence(
                absence(ctx, lhs.get_type(ctx)),
                absence(ctx, rhs.get_type(ctx)),
            );
            if union(lhs) || union(rhs) {
                let base = perry_mir::NumberStringType::get(ctx).into();
                let target = states.map_or(base, |a| {
                    perry_mir::OptionalType::get_with(ctx, base, a).into()
                });
                let lhs = self.fit(ctx, lhs, target)?;
                let rhs = self.fit(ctx, rhs, target)?;
                return Ok(self.compare(ctx, equality, lhs, rhs));
            }
            if let Some(states) = states {
                // Strict equality with `undefined` in play: compare as `T | undefined`.
                let left_inner = present_type(ctx, lhs.get_type(ctx));
                let right_inner = present_type(ctx, rhs.get_type(ctx));
                ensure!(
                    left_inner == right_inner,
                    "comparison of different types is not supported"
                );
                let target = perry_mir::OptionalType::get_with(ctx, left_inner, states).into();
                let lhs = self.fit(ctx, lhs, target)?;
                let rhs = self.fit(ctx, rhs, target)?;
                ensure!(
                    lhs.get_type(ctx) == rhs.get_type(ctx),
                    "comparison of different types is not supported"
                );
                return Ok(self.compare(ctx, equality, lhs, rhs));
            }
        }
        let lhs = self.arithmetic(ctx, lhs);
        let rhs = self.arithmetic(ctx, rhs);
        let (left_kind, right_kind) = (kind(ctx, lhs)?, kind(ctx, rhs)?);
        ensure!(
            left_kind != Kind::Regex && right_kind != Kind::Regex,
            "regex object identity is outside the literal matching profile"
        );
        ensure!(
            left_kind == right_kind,
            "comparison of different types ({} and {}) is not supported",
            left_kind.label(),
            right_kind.label()
        );
        let kind = match op {
            CompareOp::Eq => CompareKind::Eq,
            CompareOp::Ne => CompareKind::Ne,
            CompareOp::Lt => CompareKind::Lt,
            CompareOp::Le => CompareKind::Le,
            CompareOp::Gt => CompareKind::Gt,
            CompareOp::Ge => CompareKind::Ge,
            CompareOp::LooseEq | CompareOp::LooseNe => {
                bail!("loose equality is not supported; use === or !==")
            }
        };
        ensure!(
            matches!(left_kind, Kind::Number | Kind::String | Kind::BigInt)
                || matches!(kind, CompareKind::Eq | CompareKind::Ne),
            "ordered comparison is only supported on numbers and strings"
        );
        Ok(self.compare(ctx, kind, lhs, rhs))
    }

    /// Strict or loose (in)equality with a `null` or `undefined` literal: a test of the other
    /// operand's absence state. `None` when neither side is such a literal.
    fn absence_test(
        &mut self,
        ctx: &mut Context,
        equality: CompareKind,
        loose: bool,
        left: &'s Expr,
        right: &'s Expr,
    ) -> Result<Option<Value>> {
        let absent = |expr: &'s Expr| matches!(expr, Expr::Null | Expr::Undefined);
        if absent(left) && absent(right) {
            let equal = loose || (matches!(left, Expr::Null) == matches!(right, Expr::Null));
            return Ok(Some(
                self.boolean(ctx, equal == (equality == CompareKind::Eq)),
            ));
        }
        let other = match (left, right) {
            (Expr::Undefined | Expr::Null, other) | (other, Expr::Undefined | Expr::Null) => other,
            _ => return Ok(None),
        };
        let null = matches!(left, Expr::Null) || matches!(right, Expr::Null);
        let value = self.expression(ctx, other)?;
        let absent = if loose {
            self.is_nullish(ctx, value)?
        } else if let Some(json) = self.json_operand(ctx, value)? {
            let intrinsic = if null {
                Intrinsic::JsonIsNull
            } else {
                Intrinsic::JsonIsUndefined
            };
            self.intrinsic(ctx, intrinsic, vec![json])
        } else if value.get_type(ctx).deref(ctx).is::<ExceptionType>() {
            self.intrinsic(
                ctx,
                if null {
                    Intrinsic::ExceptionIsNull
                } else {
                    Intrinsic::ExceptionIsUndefined
                },
                vec![value],
            )
        } else if self.is_optional(ctx, value) {
            if null {
                self.emit_value(ctx, |ctx| IsNullOp::new(ctx, value))
            } else {
                self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value))
            }
        } else {
            self.boolean(ctx, false)
        };
        Ok(Some(self.equality_result(ctx, equality, absent)))
    }

    /// Whether `value` is `null` or `undefined`.
    pub(super) fn is_nullish(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
        if let Some(json) = self.json_operand(ctx, value)? {
            let null = self.intrinsic(ctx, Intrinsic::JsonIsNull, vec![json]);
            self.choose(
                ctx,
                ty,
                null,
                |b, ctx| Ok(b.boolean(ctx, true)),
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonIsUndefined, vec![json])),
            )
        } else if value.get_type(ctx).deref(ctx).is::<ExceptionType>() {
            let null = self.intrinsic(ctx, Intrinsic::ExceptionIsNull, vec![value]);
            self.choose(
                ctx,
                ty,
                null,
                |b, ctx| Ok(b.boolean(ctx, true)),
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::ExceptionIsUndefined, vec![value])),
            )
        } else if self.is_optional(ctx, value) {
            let undef = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            self.choose(
                ctx,
                ty,
                undef,
                |b, ctx| Ok(b.boolean(ctx, true)),
                |b, ctx| Ok(b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value))),
            )
        } else {
            Ok(self.boolean(ctx, false))
        }
    }

    /// `equal` for `===` (`CompareKind::Eq`), its negation for `!==`.
    fn equality_result(&mut self, ctx: &mut Context, equality: CompareKind, equal: Value) -> Value {
        if equality == CompareKind::Eq {
            equal
        } else {
            self.not(ctx, equal)
        }
    }

    /// The negation of a boolean.
    pub(super) fn not(&mut self, ctx: &mut Context, value: Value) -> Value {
        let no = self.boolean(ctx, false);
        self.compare(ctx, CompareKind::Eq, value, no)
    }

    /// `left && right`, `left || right` over truthiness, and `left ?? right` over nullishness.
    pub(super) fn logical(
        &mut self,
        ctx: &mut Context,
        op: &LogicalOp,
        left: &'s Expr,
        right: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let op = match op {
            LogicalOp::And => ShortCircuit::And,
            LogicalOp::Or => ShortCircuit::Or,
            LogicalOp::Coalesce => return self.coalesce(ctx, left, right, expected),
        };
        let i1_ty: TypeHandle = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let lhs = if expected == Some(i1_ty) {
            self.condition(ctx, left)?
        } else {
            self.expression(ctx, left)?
        };
        if expected == Some(i1_ty) || (expected.is_none() && lhs.get_type(ctx) == i1_ty) {
            let evaluate_rhs = match op {
                ShortCircuit::And => lhs,
                ShortCircuit::Or => self.not(ctx, lhs),
            };
            return self.choose(
                ctx,
                i1_ty,
                evaluate_rhs,
                |b, ctx| {
                    let mark = b.variant_guards.len();
                    b.variant_condition(left, op == ShortCircuit::And);
                    let rhs = b.condition(ctx, right);
                    b.variant_guards.truncate(mark);
                    rhs
                },
                |_, _| Ok(lhs),
            );
        }

        let cond = self.truthy(ctx, lhs)?;
        let truthy_block = self.new_block(ctx, "logical_truthy");
        let falsy_block = self.new_block(ctx, "logical_falsy");
        self.branch(ctx, cond, truthy_block, falsy_block);

        let (short_block, rhs_block) = match op {
            ShortCircuit::Or => (truthy_block, falsy_block),
            ShortCircuit::And => (falsy_block, truthy_block),
        };
        self.switch_to(short_block);
        let lhs = if op == ShortCircuit::Or {
            self.defined(ctx, lhs)?
        } else {
            lhs
        };
        let short_end = self.block;
        self.switch_to(rhs_block);
        let rhs_expected = expected.or_else(|| {
            Some(expression_hint(
                ctx,
                right,
                present_type(ctx, lhs.get_type(ctx)),
            ))
        });
        let mark = self.variant_guards.len();
        self.variant_condition(left, op == ShortCircuit::And);
        let rhs = self.expression_with(ctx, right, rhs_expected);
        self.variant_guards.truncate(mark);
        let rhs = rhs?;
        let rhs_end = self.block;
        let boolean =
            expected.is_none() && rhs.get_type(ctx) == i1_ty && lhs.get_type(ctx) != i1_ty;
        let result_ty = if boolean {
            i1_ty
        } else if let Some(ty) = expected {
            ty
        } else {
            let left_ty = lhs.get_type(ctx);
            let right_ty = rhs.get_type(ctx);
            let inner = match op {
                ShortCircuit::Or => present_type(ctx, left_ty),
                ShortCircuit::And => optional_value(ctx, right_ty)
                    .or_else(|| optional_value(ctx, left_ty))
                    .unwrap_or(right_ty),
            };
            merged_type(ctx, inner, left_ty, right_ty)
        };

        self.switch_to(short_end);
        let left_ty = lhs.get_type(ctx);
        let lhs = if boolean {
            self.boolean(ctx, op == ShortCircuit::Or)
        } else if op == ShortCircuit::And
            && optional_value(ctx, left_ty).is_some()
            && optional_value(ctx, left_ty) != optional_value(ctx, result_ty)
            && let Some((_, states)) = optional_parts(ctx, result_ty)
        {
            match states {
                perry_mir::Absence::Null => self.emit_value(ctx, |ctx| NullOp::new(ctx, result_ty)),
                perry_mir::Absence::Undefined => {
                    self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, result_ty))
                }
                perry_mir::Absence::Both => {
                    let is_null = self.emit_value(ctx, |ctx| IsNullOp::new(ctx, lhs));
                    self.choose(
                        ctx,
                        result_ty,
                        is_null,
                        |b, ctx| Ok(b.emit_value(ctx, |ctx| NullOp::new(ctx, result_ty))),
                        |b, ctx| Ok(b.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, result_ty))),
                    )?
                }
            }
        } else {
            self.fit(ctx, lhs, result_ty)?
        };
        let [value] =
            self.merge_branches(ctx, [result_ty], [(self.block, [lhs]), (rhs_end, [rhs])])?;
        Ok(value)
    }

    /// `left ?? right`: evaluates `right` only when `left` is `null` or `undefined`.
    fn coalesce(
        &mut self,
        ctx: &mut Context,
        left: &'s Expr,
        right: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        if matches!(left, Expr::Null | Expr::Undefined) {
            return self.expression_with(ctx, right, expected);
        }
        let lhs = self.expression(ctx, left)?;
        if !self.is_optional(ctx, lhs) && kind(ctx, lhs)? != Kind::Json {
            return match expected {
                Some(ty) => self.fit(ctx, lhs, ty),
                None => Ok(lhs),
            };
        }
        let nullish = self.is_nullish(ctx, lhs)?;
        let present = self.new_block(ctx, "coalesce_present");
        let fallback = self.new_block(ctx, "coalesce_fallback");
        self.branch(ctx, nullish, fallback, present);
        self.switch_to(present);
        let defined_lhs = self.defined(ctx, lhs)?;
        let present_end = self.block;
        self.switch_to(fallback);
        let rhs_expected =
            expected.or_else(|| Some(expression_hint(ctx, right, defined_lhs.get_type(ctx))));
        let rhs = self.expression_with(ctx, right, rhs_expected)?;
        let fallback_end = self.block;
        let left_ty = defined_lhs.get_type(ctx);
        let result_ty =
            expected.unwrap_or_else(|| merged_type(ctx, left_ty, left_ty, rhs.get_type(ctx)));
        let [value] = self.merge_branches(
            ctx,
            [result_ty],
            [(present_end, [defined_lhs]), (fallback_end, [rhs])],
        )?;
        Ok(value)
    }

    /// `!x`, `-x` and `+x`.
    pub(super) fn unary(
        &mut self,
        ctx: &mut Context,
        op: &UnaryOp,
        operand: &'s Expr,
    ) -> Result<Value> {
        if *op == UnaryOp::Not {
            let value = self.condition(ctx, operand)?;
            return Ok(self.not(ctx, value));
        }
        let value = self.expression(ctx, operand)?;
        let value = self.arithmetic(ctx, value);
        Ok(match (op, kind(ctx, value)?) {
            (UnaryOp::Neg, Kind::BigInt) => {
                self.intrinsic(ctx, Intrinsic::BigIntNegate, vec![value])
            }
            (UnaryOp::Neg, Kind::Number) => {
                // `0 - x` loses the sign of zero; `-1 * x` keeps IEEE negation.
                let minus_one = self.number(ctx, -1.0);
                self.binary(ctx, BinaryKind::Mul, minus_one, value)
            }
            (UnaryOp::Pos, Kind::Number) => value,
            (UnaryOp::Pos, Kind::String) => {
                self.intrinsic(ctx, Intrinsic::StringToNumber, vec![value])
            }
            (op, _) => bail!("unary {op:?} on this type is not supported yet"),
        })
    }

    /// The truthiness of `expr`, as a branch condition.
    pub(super) fn condition(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        if matches!(
            expr,
            Expr::Logical {
                op: LogicalOp::And | LogicalOp::Or,
                ..
            }
        ) {
            let i1_ty: TypeHandle = IntegerType::get(ctx, 1, Signedness::Signless).into();
            return self.expression_with(ctx, expr, Some(i1_ty));
        }
        let value = self.expression(ctx, expr)?;
        self.truthy(ctx, value)
    }

    /// ECMAScript `ToBoolean`.
    pub(super) fn truthy(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if self.is_optional(ctx, value) {
            let ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
            return self.match_optional(
                ctx,
                value,
                ty,
                |b, ctx| Ok(b.boolean(ctx, false)),
                |b, ctx| Ok(b.boolean(ctx, false)),
                |b, ctx, defined| b.truthy(ctx, defined),
            );
        }
        match kind(ctx, value)? {
            Kind::Boolean => Ok(value),
            Kind::String => {
                let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, value));
                let zero = self.i32(ctx, 0);
                Ok(self.compare(ctx, CompareKind::Ne, length, zero))
            }
            Kind::Number => {
                let zero = self.number(ctx, 0.0);
                let absolute = self.intrinsic(ctx, Intrinsic::MathAbs, vec![value]);
                Ok(self.compare(ctx, CompareKind::Gt, absolute, zero))
            }
            Kind::Json => Ok(self.intrinsic(ctx, Intrinsic::JsonTruthy, vec![value])),
            Kind::BigInt | Kind::NumberString => {
                bail!("narrow this scalar before testing truthiness")
            }
            _ => Ok(self.boolean(ctx, true)),
        }
    }
}
