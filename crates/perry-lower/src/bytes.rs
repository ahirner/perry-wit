//! Typed ArrayBuffer and Uint8Array construction, views, and copies.
use super::*;

impl FunctionBuilder<'_> {
    pub(super) fn byte_number(
        &mut self,
        ctx: &mut Context,
        expr: Option<&Expr>,
        default: f64,
    ) -> Result<Value> {
        match expr {
            None | Some(Expr::Undefined) => Ok(self.number(ctx, default)),
            Some(Expr::Null) => Ok(self.number(ctx, 0.0)),
            Some(expr) => {
                let value = self.expression(ctx, expr)?;
                let value = self.arithmetic(ctx, value);
                ensure!(
                    kind(ctx, value)? == Kind::Number,
                    "byte indices and lengths must be numbers"
                );
                Ok(value)
            }
        }
    }

    fn byte_end(&mut self, ctx: &mut Context, expr: Option<&Expr>) -> Result<Value> {
        let number = FP64Type::get(ctx).into();
        match expr {
            None | Some(Expr::Undefined) => {
                Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)))
            }
            Some(Expr::Null) => {
                let zero = self.number(ctx, 0.0);
                Ok(self.optional(ctx, zero))
            }
            Some(expr) => {
                let value = self.expression(ctx, expr)?;
                ensure!(
                    value.get_type(ctx) == number
                        || optional_value(ctx, value.get_type(ctx)) == Some(number),
                    "byte endpoint must be a number or undefined"
                );
                Ok(self.optional(ctx, value))
            }
        }
    }

    fn byte_source(&mut self, ctx: &mut Context, source: &Expr) -> Result<Value> {
        let number = FP64Type::get(ctx).into();
        let array = ArrayType::get(ctx, number).into();
        let source = self.expression_with(ctx, source, Some(array))?;
        self.defined(ctx, source)
    }

    pub(super) fn bytes_constructor(&mut self, ctx: &mut Context, args: &[&Expr]) -> Result<Value> {
        ensure!(args.len() <= 3, "Uint8Array takes at most three arguments");
        let Some(source) = args.first() else {
            let zero = self.number(ctx, 0.0);
            return Ok(self
                .intrinsic(ctx, Intrinsic::BytesNew, vec![zero])
                .unwrap());
        };
        let source = match source {
            Expr::Undefined | Expr::Null => self.number(ctx, 0.0),
            source => {
                let number = FP64Type::get(ctx).into();
                let array = ArrayType::get(ctx, number).into();
                let value = self.expression_with(ctx, source, Some(array))?;
                if optional_value(ctx, value.get_type(ctx)) == Some(number) {
                    self.arithmetic(ctx, value)
                } else {
                    self.defined(ctx, value)?
                }
            }
        };
        if kind(ctx, source)? == Kind::Buffer {
            let offset = self.byte_number(ctx, args.get(1).copied(), 0.0)?;
            let length = self.byte_end(ctx, args.get(2).copied())?;
            return Ok(self
                .intrinsic(ctx, Intrinsic::BytesView, vec![source, offset, length])
                .unwrap());
        }
        ensure!(
            args.len() == 1,
            "Uint8Array offset and length require an ArrayBuffer"
        );
        let op = match kind(ctx, source)? {
            Kind::Number => Intrinsic::BytesNew,
            Kind::Bytes => Intrinsic::BytesCopy,
            Kind::Array => {
                ensure!(
                    self.array_element(ctx, source)?.deref(ctx).is::<FP64Type>(),
                    "Uint8Array elements must be numbers"
                );
                Intrinsic::BytesFromArray
            }
            _ => bail!("Uint8Array needs a length, numeric array, byte view, or ArrayBuffer"),
        };
        Ok(self.intrinsic(ctx, op, vec![source]).unwrap())
    }

    pub(super) fn bytes_from(&mut self, ctx: &mut Context, source: &Expr) -> Result<Value> {
        let source = self.byte_source(ctx, source)?;
        let op = match kind(ctx, source)? {
            Kind::Bytes => Intrinsic::BytesCopy,
            Kind::Array => {
                ensure!(
                    self.array_element(ctx, source)?.deref(ctx).is::<FP64Type>(),
                    "Uint8Array.from elements must be numbers"
                );
                Intrinsic::BytesFromArray
            }
            _ => bail!("Uint8Array.from requires a numeric array or byte view"),
        };
        Ok(self.intrinsic(ctx, op, vec![source]).unwrap())
    }

    pub(super) fn bytes_method(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<Value>> {
        match method {
            "subarray" | "slice" => {
                ensure!(
                    args.len() <= 2,
                    "Uint8Array.{method} takes at most two arguments"
                );
                let begin = self.byte_number(ctx, args.first(), 0.0)?;
                let end = self.byte_end(ctx, args.get(1))?;
                let op = if method == "slice" {
                    Intrinsic::BytesSlice
                } else {
                    Intrinsic::BytesSubarray
                };
                Ok(self.intrinsic(ctx, op, vec![receiver, begin, end]))
            }
            "set" => {
                ensure!(
                    !args.is_empty() && args.len() <= 2,
                    "Uint8Array.set takes a source and optional offset"
                );
                let source = self.byte_source(ctx, &args[0])?;
                let offset = self.byte_number(ctx, args.get(1), 0.0)?;
                let op = match kind(ctx, source)? {
                    Kind::Bytes => Intrinsic::BytesSet,
                    Kind::Array => {
                        ensure!(
                            self.array_element(ctx, source)?.deref(ctx).is::<FP64Type>(),
                            "Uint8Array.set source must contain numbers"
                        );
                        Intrinsic::BytesSetArray
                    }
                    _ => bail!("Uint8Array.set source must be a byte view or numeric array"),
                };
                Ok(self.intrinsic(ctx, op, vec![receiver, source, offset]))
            }
            _ => bail!("Uint8Array.{method} is not supported yet"),
        }
    }
}
