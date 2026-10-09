//! Allocation-free lowering of the closed string/byte-view union.
use super::*;
impl FunctionLowering<'_> {
    pub(super) fn text_bytes_is_text(&mut self, ctx: &mut Context, value: Value) -> Value {
        let tag = self.i64_const(ctx, perry_runtime::text_bytes::BYTES_TAG);
        let kind = self.num(ctx, NumOpcode::I64And, vec![value, tag]);
        self.num(ctx, NumOpcode::I64Eqz, vec![kind])
    }
    pub(super) fn lower_union_op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<bool> {
        use perry_mir::unions::{InjectOp, ProjectOp};
        if Operation::get_op::<InjectOp>(op, ctx).is_some() {
            let ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let value = self.operand(ctx, op, 0)?;
            let mut bits = self.num(ctx, NumOpcode::I64ExtendI32U, vec![value]);
            if optional_inner(ctx, ty)
                .unwrap_or(ty)
                .deref(ctx)
                .is::<BytesType>()
            {
                let tag = self.i64_const(ctx, perry_runtime::text_bytes::BYTES_TAG);
                let tagged = self.num(ctx, NumOpcode::I64Or, vec![bits, tag]);
                bits = if optional_inner(ctx, ty).is_some() {
                    self.select(ctx, value, tagged, bits)
                } else {
                    tagged
                };
            }
            self.define(ctx, op, bits);
        } else if Operation::get_op::<ProjectOp>(op, ctx).is_some() {
            let ty = op.deref(ctx).get_result(0).get_type(ctx);
            let value = self.operand(ctx, op, 0)?;
            let text = self.text_bytes_is_text(ctx, value);
            let optional = optional_inner(ctx, ty);
            let wrong = if optional.unwrap_or(ty).deref(ctx).is::<StringType>() {
                self.num(ctx, NumOpcode::I32Eqz, vec![text])
            } else {
                text
            };
            let wrong = if optional.is_some() {
                let zero = self.i64_const(ctx, 0);
                let present = self.num(ctx, NumOpcode::I64Ne, vec![value, zero]);
                self.num(ctx, NumOpcode::I32And, vec![wrong, present])
            } else {
                wrong
            };
            self.throw_if(ctx, wrong, ErrorKind::TypeError);
            let value = self.num(ctx, NumOpcode::I32WrapI64, vec![value]);
            self.define(ctx, op, value);
        } else if Operation::get_op::<IntrinsicOp>(op, ctx)
            .is_some_and(|op| op.kind(ctx) == Intrinsic::TextBytesIsText)
        {
            let value = self.operand(ctx, op, 0)?;
            let value = self.text_bytes_is_text(ctx, value);
            self.define(ctx, op, value);
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}
