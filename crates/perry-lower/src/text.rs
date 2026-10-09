//! Codec calls preserve argument effects and remain distinct from JSON or numeric handles.
use super::*;

impl FunctionBuilder<'_> {
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
        let label = self.optional_argument(ctx, args.first())?;
        let flags = self.literal_options(ctx, args.get(1), &["fatal", "ignoreBOM"])?;
        let string = StringType::get(ctx).into();
        let label = self.fit_optional(ctx, label, string)?;
        let fatal = self.boolean_option(ctx, flags[0], true)?;
        let ignore = self.boolean_option(ctx, flags[1], false)?;
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
                let source = self.optional_argument(ctx, args.first())?;
                let string = StringType::get(ctx).into();
                let source = self.fit_optional(ctx, source, string)?;
                Ok(self.intrinsic(ctx, Intrinsic::TextEncode, vec![source]))
            }
            (Kind::TextDecoder, "decode") => {
                ensure!(
                    args.len() <= 2,
                    "TextDecoder.decode takes input and options"
                );
                let input = self.optional_argument(ctx, args.first())?;
                let options = self.literal_options(ctx, args.get(1), &["stream"])?;
                let bytes = BytesType::get(ctx).into();
                let input = self.fit_optional(ctx, input, bytes)?;
                let stream = self.boolean_option(ctx, options[0], false)?;
                Ok(self.intrinsic(ctx, Intrinsic::TextDecode, vec![receiver, input, stream]))
            }
            _ => bail!("codec method {method} is not supported yet"),
        }
    }
}
