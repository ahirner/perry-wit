//! TextDecoder semantics.
use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn decode(
        &mut self,
        ctx: &mut Context,
        decoder: Value,
        input: Option<&'s Expr>,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let bytes = match input {
            None | Some(Expr::Undefined) => {
                let zero = self.number(ctx, 0.0);
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![zero])
            }
            Some(input) => self.typed_operand(ctx, input, Kind::Bytes, "a Uint8Array")?,
        };
        let mut stream = self.boolean(ctx, false);
        for (name, value) in self.init_fields(options)? {
            ensure!(
                name == "stream",
                "unsupported TextDecoder.decode option {name}"
            );
            stream = self.condition(ctx, value)?;
        }
        Ok(self.intrinsic(ctx, Intrinsic::TextDecode, vec![decoder, bytes, stream]))
    }
}
