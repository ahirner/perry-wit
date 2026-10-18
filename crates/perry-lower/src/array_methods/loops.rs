use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn dense_loop<T>(
        &mut self,
        ctx: &mut Context,
        iteration: ArrayIteration,
        body: impl FnOnce(
            &mut Self,
            &mut Context,
            Value,
            Value,
            Ptr<BasicBlock>,
            Ptr<BasicBlock>,
        ) -> Result<T>,
    ) -> Result<T> {
        let ArrayIteration {
            array,
            start,
            limit,
            direction,
            holes,
        } = iteration;
        self.index_loop(ctx, "array", start, limit, |b, ctx, step, next, done| {
            let current = match direction {
                Direction::Forward => step,
                Direction::Reverse => {
                    let one = b.number(ctx, 1.0);
                    let last = b.binary(ctx, BinaryKind::Sub, limit, one);
                    b.binary(ctx, BinaryKind::Sub, last, step)
                }
            };
            let available = b.array_length(ctx, array);
            let present = b.compare(ctx, CompareKind::Lt, current, available);
            let item = match holes {
                Holes::Skip => {
                    let read = b.new_block(ctx, "array_read");
                    b.branch(ctx, present, read, next);
                    b.switch_to(read);
                    b.element_at(ctx, array, current)?
                }
                Holes::Visit(ty) => b.choose(
                    ctx,
                    ty,
                    present,
                    |b, ctx| {
                        let item = b.element_at(ctx, array, current)?;
                        b.fit(ctx, item, ty)
                    },
                    |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))),
                )?,
            };
            body(b, ctx, item, current, next, done)
        })
    }

    /// `for (i = start; i < limit; i++) body` over `f64` indices. `body` gets `i`, the block
    /// that advances to the next index, and the loop's exit, where lowering continues.
    pub(crate) fn index_loop<T>(
        &mut self,
        ctx: &mut Context,
        label: &str,
        start: Value,
        limit: Value,
        body: impl FnOnce(&mut Self, &mut Context, Value, Ptr<BasicBlock>, Ptr<BasicBlock>) -> Result<T>,
    ) -> Result<T> {
        let index = self.entry_slot(ctx, start.get_type(ctx));
        self.store(ctx, index, start);
        let header = self.new_block(ctx, &format!("{label}_header"));
        let entered = self.new_block(ctx, &format!("{label}_body"));
        let next = self.new_block(ctx, &format!("{label}_next"));
        let done = self.new_block(ctx, &format!("{label}_done"));
        self.jump(ctx, header);
        self.switch_to(header);
        let current = self.load(ctx, index)?;
        let more = self.compare(ctx, CompareKind::Lt, current, limit);
        self.branch(ctx, more, entered, done);
        self.switch_to(entered);
        let output = body(self, ctx, current, next, done)?;
        self.jump(ctx, next);
        self.switch_to(next);
        let one = self.number(ctx, 1.0);
        let advanced = self.binary(ctx, BinaryKind::Add, current, one);
        self.store(ctx, index, advanced);
        self.jump(ctx, header);
        self.switch_to(done);
        Ok(output)
    }

    /// `array[index]` for an `f64` index the caller checked against the length.
    pub(crate) fn element_at(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
    ) -> Result<Value> {
        let at = self.emit_value(ctx, |ctx| {
            NumericCastOp::new(
                ctx,
                index,
                IntegerType::get(ctx, 32, Signedness::Signless).into(),
            )
        });
        let op = ArrayAtOp::new(ctx, array, at)?;
        Ok(self.append_value(ctx, &op))
    }
}
