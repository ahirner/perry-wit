//! Operators: arithmetic, comparison, logical, unary and truthiness.
use super::*;
use crate::coerce::merge_absence;

impl<'s> FunctionBuilder<'s> {
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
            optional_value(ctx, value.get_type(ctx))
                .unwrap_or(value.get_type(ctx))
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
            (op @ (BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul), Kind::BigInt, Kind::BigInt) => {
                let kind = match op {
                    BinaryOp::Add => BinaryKind::Add,
                    BinaryOp::Sub => BinaryKind::Sub,
                    _ => BinaryKind::Mul,
                };
                self.binary(ctx, kind, lhs, rhs)
            }
            (op, _, _) => {
                bail!("operator {op:?} on these operand types is not supported yet")
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
        let equality = match op {
            CompareOp::Eq => Some(CompareKind::Eq),
            CompareOp::Ne => Some(CompareKind::Ne),
            _ => None,
        };
        if let Some(equality) = equality
            && let Some(value) = self.absence_test(ctx, equality, left, right)?
        {
            return Ok(value);
        }
        let lhs = self.expression(ctx, left)?;
        let rhs = self.expression(ctx, right)?;
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
                    if value
                        .get_type(ctx)
                        .deref(ctx)
                        .is::<perry_mir::types::BigIntType>()
                    {
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
                optional_value(ctx, value.get_type(ctx))
                    .unwrap_or(value.get_type(ctx))
                    .deref(ctx)
                    .is::<perry_mir::types::NumberStringType>()
            };
            let states = merge_absence(
                absence(ctx, lhs.get_type(ctx)),
                absence(ctx, rhs.get_type(ctx)),
            );
            if union(lhs) || union(rhs) {
                let base = perry_mir::types::NumberStringType::get(ctx).into();
                let target = states.map_or(base, |a| {
                    perry_mir::types::OptionalType::get_with(ctx, base, a).into()
                });
                let lhs = self.fit(ctx, lhs, target)?;
                let rhs = self.fit(ctx, rhs, target)?;
                return Ok(self.compare(ctx, equality, lhs, rhs));
            }
            if let Some(states) = states {
                // Strict equality with `undefined` in play: compare as `T | undefined`.
                let left_inner =
                    optional_value(ctx, lhs.get_type(ctx)).unwrap_or(lhs.get_type(ctx));
                let right_inner =
                    optional_value(ctx, rhs.get_type(ctx)).unwrap_or(rhs.get_type(ctx));
                ensure!(
                    left_inner == right_inner,
                    "comparison of different types is not supported"
                );
                let target =
                    perry_mir::types::OptionalType::get_with(ctx, left_inner, states).into();
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
            "comparison of different types is not supported"
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

    /// Strict (in)equality with a `null` or `undefined` literal: a test of the other
    /// operand's absence state. `None` when neither side is such a literal.
    fn absence_test(
        &mut self,
        ctx: &mut Context,
        equality: CompareKind,
        left: &'s Expr,
        right: &'s Expr,
    ) -> Result<Option<Value>> {
        let absent = |expr: &'s Expr| matches!(expr, Expr::Null | Expr::Undefined);
        if absent(left) && absent(right) {
            let equal = matches!(left, Expr::Null) == matches!(right, Expr::Null);
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
        let absent = if let Some(json) = self.json_operand(ctx, value)? {
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

    /// `equal` for `===` (`CompareKind::Eq`), its negation for `!==`.
    fn equality_result(&mut self, ctx: &mut Context, equality: CompareKind, equal: Value) -> Value {
        if equality == CompareKind::Eq {
            equal
        } else {
            self.not(ctx, equal)
        }
    }

    /// The negation of a boolean.
    fn not(&mut self, ctx: &mut Context, value: Value) -> Value {
        let no = self.boolean(ctx, false);
        self.compare(ctx, CompareKind::Eq, value, no)
    }

    /// `left && right` and `left || right` over truthiness.
    pub(super) fn logical(
        &mut self,
        ctx: &mut Context,
        op: &LogicalOp,
        left: &'s Expr,
        right: &'s Expr,
    ) -> Result<Value> {
        // Short-circuit through a result slot; mem2reg turns it into a block argument.
        ensure!(
            !matches!(op, LogicalOp::Coalesce),
            "?? is not supported yet"
        );
        let lhs = self.condition(ctx, left)?;
        let result = self.entry_slot(ctx, lhs.get_type(ctx));
        self.store(ctx, result, lhs);
        let rhs_block = self.new_block(ctx, "rhs");
        let join = self.new_block(ctx, "join");
        match op {
            LogicalOp::And => self.branch(ctx, lhs, rhs_block, join),
            _ => self.branch(ctx, lhs, join, rhs_block),
        }
        self.switch_to(rhs_block);
        let rhs = self.condition(ctx, right)?;
        self.store(ctx, result, rhs);
        self.jump(ctx, join);
        self.switch_to(join);
        self.load(ctx, result)
    }

    /// `!x`, `-x` and `+x`.
    pub(super) fn unary(
        &mut self,
        ctx: &mut Context,
        op: &UnaryOp,
        operand: &'s Expr,
    ) -> Result<Value> {
        let value = self.expression(ctx, operand)?;
        if *op == UnaryOp::Not {
            let value = self.truthy(ctx, value)?;
            return Ok(self.not(ctx, value));
        }
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
        let value = self.expression(ctx, expr)?;
        self.truthy(ctx, value)
    }

    /// ECMAScript `ToBoolean`.
    pub(super) fn truthy(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if self.is_optional(ctx, value) {
            let absent = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            let ty = IntegerType::get(ctx, 1, Signedness::Signless).into();
            return self.choose(
                ctx,
                ty,
                absent,
                |b, ctx| Ok(b.boolean(ctx, false)),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                    b.choose(
                        ctx,
                        ty,
                        null,
                        |b, ctx| Ok(b.boolean(ctx, false)),
                        |b, ctx| {
                            let value = b.defined(ctx, value)?;
                            b.truthy(ctx, value)
                        },
                    )
                },
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
