use super::*;

impl<'s> FunctionBuilder<'s> {
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
    pub(crate) fn array_for_each(
        &mut self,
        ctx: &mut Context,
        arr: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        let arr = self.defined(ctx, arr)?;
        self.array_element(ctx, arr)?;
        self.iterate(
            ctx,
            Input::Array(arr, Holes::Skip),
            |b, ctx, item, idx, _, _| {
                b.array_callback(ctx, callback, &[item, idx, arr])?;
                Ok(())
            },
        )?;
        let ty = void_type(ctx);
        Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
    }
    pub(crate) fn retain_array(
        &mut self,
        ctx: &mut Context,
        array: Value,
        keep: impl FnOnce(&mut Self, &mut Context, Value) -> Result<Value>,
    ) -> Result<Value> {
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let write = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, write, zero);
        let limit = self.array_length(ctx, array);
        self.index_loop(ctx, "retain", zero, limit, |b, ctx, index, next, _| {
            let item = b.element_at(ctx, array, index)?;
            let keep = keep(b, ctx, item)?;
            let retained = b.new_block(ctx, "retained");
            b.branch(ctx, keep, retained, next);
            b.switch_to(retained);
            let position = b.load(ctx, write)?;
            b.emit(ctx, |ctx| ArraySetOp::new(ctx, array, position, item));
            let advanced = b.binary(ctx, BinaryKind::Add, position, one);
            b.store(ctx, write, advanced);
            Ok(())
        })?;
        let retained = self.load(ctx, write)?;
        let integer = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let length = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, retained, integer));
        self.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, array, length));
        Ok(self.compare(ctx, CompareKind::Lt, retained, limit))
    }
}
