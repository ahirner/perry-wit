use super::*;

impl<'s> FunctionBuilder<'s> {
    /// `array` without holes, of the same type: [Holes::Skip] drops them and [Holes::Dense]
    /// reads them through [Self::hole_as_element]. A dense `array` is returned itself unless
    /// `copy` is set.
    pub(crate) fn without_holes(
        &mut self,
        ctx: &mut Context,
        array: Value,
        holes: Holes,
        copy: bool,
    ) -> Result<Value> {
        let dense = |b: &mut Self, ctx: &mut Context| {
            Ok(if copy {
                b.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![array]))
            } else {
                array
            })
        };
        if !self.shapes.metadata.sparse_arrays {
            return dense(self, ctx);
        }
        let element = self.array_element(ctx, array)?;
        let is_dense = self.emit_value(ctx, |ctx| perry_mir::ArrayIsDenseOp::new(ctx, array));
        self.choose(ctx, array.get_type(ctx), is_dense, dense, |b, ctx| {
            b.collect_items(ctx, Input::Array(array, holes), |b, ctx, item, _, _| {
                Ok(ArrayOutput::Item(b.hole_as_element(ctx, item, element)?))
            })
        })
    }

    /// View `array` with `undefined` elements when the `expected` element admits them, so its
    /// materialized holes read as `undefined` rather than throwing.
    pub(crate) fn admit_holes(
        &mut self,
        ctx: &mut Context,
        array: Value,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        if !self.shapes.metadata.sparse_arrays
            || !expected.is_some_and(|element| perry_mir::admits_undefined(ctx, element))
        {
            return Ok(array);
        }
        let element = self.array_element(ctx, array)?;
        let element = crate::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
        let target = ArrayType::get(ctx, element).into();
        self.fit(ctx, array, target)
    }

    /// A [Holes::Dense] item as `element`. A hole is `undefined`, which throws `TypeError` when
    /// `element` excludes it: a `T[]` copy stays `T[]` even if `delete` or `map` left holes.
    pub(crate) fn hole_as_element(
        &mut self,
        ctx: &mut Context,
        item: Value,
        element: TypeHandle,
    ) -> Result<Value> {
        let item = if perry_mir::admits_undefined(ctx, element) {
            item
        } else {
            self.defined(ctx, item)?
        };
        self.fit(ctx, item, element)
    }

    pub(crate) fn copy_array_slot(
        &mut self,
        ctx: &mut Context,
        target: Value,
        index: Value,
        value: Value,
        present: Value,
    ) -> Result<()> {
        let write = self.new_block(ctx, "copy_present");
        let hole = self.new_block(ctx, "copy_hole");
        let done = self.new_block(ctx, "copied_slot");
        self.branch(ctx, present, write, hole);
        self.switch_to(write);
        let element = self.array_element(ctx, target)?;
        let value = self.fit(ctx, value, element)?;
        self.emit(ctx, |ctx| ArraySetOp::new(ctx, target, index, value));
        self.jump(ctx, done);
        self.switch_to(hole);
        self.emit(ctx, |ctx| perry_mir::ArrayHoleOp::new(ctx, target, index));
        self.jump(ctx, done);
        self.switch_to(done);
        Ok(())
    }
    pub(crate) fn array_value(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
    ) -> Result<Value> {
        if !self.shapes.metadata.sparse_arrays {
            return self.element_at(ctx, array, index);
        }
        let op = ArrayGetOp::new(ctx, array, index)?;
        Ok(self.append_value(ctx, &op))
    }

    pub(crate) fn collect_preserving_holes(
        &mut self,
        ctx: &mut Context,
        array: Value,
        start: Value,
        limit: Value,
        item: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<Value>,
    ) -> Result<Value> {
        let prepare = self.block;
        self.index_loop(
            ctx,
            "preserve_holes",
            start,
            limit,
            |b, ctx, index, _, _| {
                let present =
                    b.emit_value(ctx, |ctx| perry_mir::ArrayPresentOp::new(ctx, array, index));
                let read = b.new_block(ctx, "present_element");
                let hole = b.new_block(ctx, "missing_element");
                let done = b.new_block(ctx, "collected_element");
                b.branch(ctx, present, read, hole);
                b.switch_to(read);
                let value = b.element_at(ctx, array, index)?;
                let value = item(b, ctx, value, index)?;
                let allocation = ArrayNewOp::new(ctx, value.get_type(ctx), vec![]);
                crate::builder::insert_before_terminator(
                    ctx,
                    prepare,
                    [allocation.get_operation()],
                )?;
                let result = allocation.get_result(ctx);
                b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, value));
                b.jump(ctx, done);
                b.switch_to(hole);
                let position = b.array_length(ctx, result);
                b.emit(ctx, |ctx| {
                    perry_mir::ArrayHoleOp::new(ctx, result, position)
                });
                b.jump(ctx, done);
                b.switch_to(done);
                Ok(result)
            },
        )
    }

    /// `array[index]` for an `f64` index the caller proved present and in bounds.
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
