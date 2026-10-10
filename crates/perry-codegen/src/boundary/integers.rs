//! WIT s64/u64 transport never passes through a floating-point representation.
use super::*;

impl FunctionLowering<'_> {
    pub(super) fn lift_integer(
        &mut self,
        ctx: &mut Context,
        signed: bool,
        mir: TypeHandle,
        raw: Value,
    ) -> Result<Value> {
        ensure!(
            mir.deref(ctx).is::<perry_mir::types::BigIntType>(),
            "WIT 64-bit integers require TypeScript bigint; use Number(value) for an explicit lossy conversion"
        );
        let zero = self.i64_const(ctx, 0);
        let negative = if signed {
            self.num(ctx, NumOpcode::I64LtS, vec![raw, zero])
        } else {
            self.i32_const(ctx, 0)
        };
        let inverted = self.num(ctx, NumOpcode::I64Sub, vec![zero, raw]);
        let magnitude = self.select(ctx, negative, inverted, raw);
        let size = self.i32_const(ctx, 16);
        let value = self.alloc(ctx, size);
        self.store(ctx, MemKind::I64, value, 0, magnitude);
        self.store(ctx, MemKind::I32, value, 8, negative);
        Ok(value)
    }

    pub(super) fn lower_integer(
        &mut self,
        ctx: &mut Context,
        signed: bool,
        mir: TypeHandle,
        value: Value,
    ) -> Result<Value> {
        ensure!(
            mir.deref(ctx).is::<perry_mir::types::BigIntType>(),
            "WIT 64-bit integers require TypeScript bigint"
        );
        let magnitude = self.load(ctx, MemKind::I64, value, 0);
        let negative = self.load(ctx, MemKind::I32, value, 8);
        let invalid = if signed {
            let max_positive = self.i64_const(ctx, i64::MAX as u64);
            let max_negative = self.i64_const(ctx, i64::MIN as u64);
            let limit = self.select(ctx, negative, max_negative, max_positive);
            self.num(ctx, NumOpcode::I64GtU, vec![magnitude, limit])
        } else {
            negative
        };
        // An invalid canonical value cannot cross the boundary. Compiled bigint arithmetic
        // remains outside the transport domain, so overflow is never silently truncated.
        let valid = self.new_block(ctx, vec![]);
        let trap = self.new_block(ctx, vec![]);
        self.cond_br(ctx, invalid, (trap, vec![]), (valid, vec![]));
        self.block = trap;
        let op = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, op);
        self.block = valid;
        let zero = self.i64_const(ctx, 0);
        let inverted = self.num(ctx, NumOpcode::I64Sub, vec![zero, magnitude]);
        Ok(self.select(ctx, negative, inverted, magnitude))
    }
}
