use super::*;
impl FunctionBuilder<'_> {
    pub(super) fn decode_json(
        &mut self,
        ctx: &mut Context,
        input: Value,
        target: TypeHandle,
    ) -> Result<Value> {
        ensure!(
            perry_mir::JsonSchema::supports_descriptor(ctx, target),
            "unsupported JSON schema"
        );
        let op = perry_mir::JsonDecodeOp::new(ctx, input, target);
        self.append(ctx, &op);
        Ok(op.get_operation().deref(ctx).get_result(0))
    }
}
