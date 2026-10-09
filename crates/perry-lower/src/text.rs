//! Codec calls preserve argument effects and remain distinct from JSON or numeric handles.
use super::*;

impl FunctionBuilder<'_> {
    fn codec_argument(
        &mut self,
        ctx: &mut Context,
        expression: Option<&Expr>,
    ) -> Result<Option<Value>> {
        match expression {
            None | Some(Expr::Undefined) => Ok(None),
            Some(Expr::Call { callee, args, .. }) => self.call(ctx, callee, args),
            Some(expression) => self.expression(ctx, expression).map(Some),
        }
    }

    fn codec_optional(
        &mut self,
        ctx: &mut Context,
        value: Option<Value>,
        ty: TypeHandle,
    ) -> Result<Value> {
        let target = perry_mir::types::OptionalType::get(ctx, ty).into();
        match value {
            Some(value) => self.fit(ctx, value, target),
            None => Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))),
        }
    }

    fn codec_options(
        &mut self,
        ctx: &mut Context,
        expression: Option<&Expr>,
        names: &[&str],
    ) -> Result<Vec<Option<Value>>> {
        let mut values = vec![None; names.len()];
        let fields: Vec<(String, &Expr)> = match expression {
            None | Some(Expr::Undefined | Expr::Null) => return Ok(values),
            Some(Expr::Object(fields)) => fields
                .iter()
                .map(|(name, value)| (name.clone(), value))
                .collect(),
            Some(Expr::New {
                class_name, args, ..
            }) => self
                .shapes
                .literal_fields(class_name)
                .context("TextDecoder options must be literal data properties")?
                .into_iter()
                .zip(args)
                .collect(),
            _ => bail!("TextDecoder options must be literal data properties"),
        };
        for (name, expression) in fields {
            if let Some(index) = names.iter().position(|&known| known == name) {
                values[index] = self.codec_argument(ctx, Some(expression))?;
            } else {
                self.effect(ctx, expression)?;
            }
        }
        Ok(values)
    }

    fn codec_boolean(
        &mut self,
        ctx: &mut Context,
        value: Option<Value>,
        default: bool,
    ) -> Result<Value> {
        let Some(value) = value else {
            return Ok(self.boolean(ctx, default));
        };
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let value = self.codec_optional(ctx, Some(value), boolean)?;
        let comparison = self.boolean(ctx, !default);
        let comparison = self.optional(ctx, comparison);
        Ok(self.compare(
            ctx,
            if default {
                CompareKind::Ne
            } else {
                CompareKind::Eq
            },
            value,
            comparison,
        ))
    }

    pub(super) fn codec_constructor(
        &mut self,
        ctx: &mut Context,
        decoder: bool,
        args: &[Expr],
    ) -> Result<Value> {
        if !decoder {
            ensure!(
                args.is_empty(),
                "TextEncoder constructor takes no arguments"
            );
            return Ok(self
                .intrinsic(ctx, Intrinsic::TextEncoderNew, vec![])
                .unwrap());
        }
        ensure!(args.len() <= 2, "TextDecoder takes a label and options");
        let label = self.codec_argument(ctx, args.first())?;
        let flags = self.codec_options(ctx, args.get(1), &["fatal", "ignoreBOM"])?;
        let string = StringType::get(ctx).into();
        let label = self.codec_optional(ctx, label, string)?;
        let fatal = self.codec_boolean(ctx, flags[0], true)?;
        let ignore = self.codec_boolean(ctx, flags[1], false)?;
        Ok(self
            .intrinsic(ctx, Intrinsic::TextDecoderNew, vec![label, fatal, ignore])
            .unwrap())
    }

    pub(super) fn codec_method(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<Value>> {
        match (kind(ctx, receiver)?, method) {
            (Kind::TextEncoder, "encode") => {
                ensure!(
                    args.len() <= 1,
                    "TextEncoder.encode takes an optional string"
                );
                let source = self.codec_argument(ctx, args.first())?;
                let string = StringType::get(ctx).into();
                let source = self.codec_optional(ctx, source, string)?;
                Ok(self.intrinsic(ctx, Intrinsic::TextEncode, vec![source]))
            }
            (Kind::TextDecoder, "decode") => {
                ensure!(
                    args.len() <= 2,
                    "TextDecoder.decode takes input and options"
                );
                let input = self.codec_argument(ctx, args.first())?;
                let options = self.codec_options(ctx, args.get(1), &["stream"])?;
                let bytes = BytesType::get(ctx).into();
                let input = self.codec_optional(ctx, input, bytes)?;
                let stream = self.codec_boolean(ctx, options[0], false)?;
                Ok(self.intrinsic(ctx, Intrinsic::TextDecode, vec![receiver, input, stream]))
            }
            _ => bail!("codec method {method} is not supported yet"),
        }
    }
}
