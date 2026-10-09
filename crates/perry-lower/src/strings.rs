//! String builtins evaluate the receiver and arguments once before typed operations.
use super::*;

pub(super) fn is_string_method(method: &str) -> bool {
    matches!(
        method,
        "charAt"
            | "charCodeAt"
            | "at"
            | "codePointAt"
            | "slice"
            | "substring"
            | "indexOf"
            | "lastIndexOf"
            | "includes"
            | "startsWith"
            | "endsWith"
            | "toLowerCase"
            | "toUpperCase"
            | "trim"
            | "trimStart"
            | "trimEnd"
            | "repeat"
            | "split"
            | "concat"
            | "toString"
            | "valueOf"
            | "isWellFormed"
            | "toWellFormed"
    )
}
impl FunctionBuilder<'_> {
    fn string_number(&mut self, ctx: &mut Context, value: Option<Value>) -> Result<Value> {
        let value = self.string_optional_number(ctx, value)?;
        Ok(self.arithmetic(ctx, value))
    }
    fn string_optional_number(&mut self, ctx: &mut Context, value: Option<Value>) -> Result<Value> {
        let number = FP64Type::get(ctx).into();
        match value {
            None => Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number))),
            Some(value) => {
                let target = perry_mir::types::OptionalType::get(ctx, number).into();
                self.fit(ctx, value, target)
            }
        }
    }
    fn string_argument(&mut self, ctx: &mut Context, value: Option<Value>) -> Result<Value> {
        let value = self.string_optional_argument(ctx, value)?;
        Ok(self.arithmetic(ctx, value))
    }
    fn string_optional_argument(
        &mut self,
        ctx: &mut Context,
        value: Option<Value>,
    ) -> Result<Value> {
        let string = StringType::get(ctx).into();
        match value {
            None => Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, string))),
            Some(value) => {
                let target = perry_mir::types::OptionalType::get(ctx, string).into();
                self.fit(ctx, value, target)
            }
        }
    }
    pub(super) fn string_method(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<Value>> {
        ensure!(
            method != "charCodeAt",
            "charCodeAt is a UTF-16 code-unit API outside the supported subset; use codePointAt"
        );
        ensure!(
            is_string_method(method),
            "String.{method} is not supported yet"
        );
        let values = args
            .iter()
            .map(|arg| match arg {
                Expr::Undefined => Ok(None),
                _ => self.expression(ctx, arg).map(Some),
            })
            .collect::<Result<Vec<_>>>()?;
        let receiver = if json::is_json(ctx, receiver) {
            let string = StringType::get(ctx).into();
            self.narrow_json(ctx, receiver, string)?
        } else {
            receiver
        };
        ensure!(
            kind(ctx, receiver)? == Kind::String,
            "string method requires a string receiver"
        );
        let first = values.first().copied().flatten();
        let second = values.get(1).copied().flatten();
        let mut operands = vec![receiver];
        let op = match method {
            "charAt" | "at" | "codePointAt" => {
                operands.push(self.string_number(ctx, first)?);
                match method {
                    "charAt" => Intrinsic::StringCharAt,
                    "at" => Intrinsic::StringAt,
                    _ => Intrinsic::StringCodePointAt,
                }
            }
            "slice" | "substring" => {
                operands.push(self.string_number(ctx, first)?);
                operands.push(self.string_optional_number(ctx, second)?);
                if method == "slice" {
                    Intrinsic::StringSlice
                } else {
                    Intrinsic::StringSubstring
                }
            }
            "indexOf" | "lastIndexOf" | "includes" | "startsWith" | "endsWith" => {
                operands.push(self.string_argument(ctx, first)?);
                operands.push(self.string_optional_number(ctx, second)?);
                let op = match method {
                    "lastIndexOf" => Intrinsic::StringLastIndexOf,
                    "startsWith" => Intrinsic::StringStartsWith,
                    "endsWith" => Intrinsic::StringEndsWith,
                    _ => Intrinsic::StringIndexOf,
                };
                let value = self.intrinsic(ctx, op, operands).unwrap();
                return Ok(Some(
                    if matches!(method, "includes" | "startsWith" | "endsWith") {
                        let zero = self.number(ctx, 0.0);
                        self.compare(ctx, CompareKind::Ge, value, zero)
                    } else {
                        value
                    },
                ));
            }
            "repeat" => {
                operands.push(self.string_number(ctx, first)?);
                Intrinsic::StringRepeat
            }
            "split" => {
                operands.push(self.string_optional_argument(ctx, first)?);
                operands.push(self.string_optional_number(ctx, second)?);
                Intrinsic::StringSplit
            }
            "toLowerCase" => Intrinsic::StringLower,
            "toUpperCase" => Intrinsic::StringUpper,
            "trim" => Intrinsic::StringTrim,
            "trimStart" => Intrinsic::StringTrimStart,
            "trimEnd" => Intrinsic::StringTrimEnd,
            "concat" => {
                let mut result = receiver;
                for value in values {
                    let next = self.string_argument(ctx, value)?;
                    result = self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, result, next));
                }
                return Ok(Some(result));
            }
            "isWellFormed" => return Ok(Some(self.boolean(ctx, true))),
            "toString" | "valueOf" | "toWellFormed" => return Ok(Some(receiver)),
            _ => unreachable!("validated string method"),
        };
        Ok(self.intrinsic(ctx, op, operands))
    }
}
