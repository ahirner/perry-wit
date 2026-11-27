use super::*;

impl<'s> FunctionBuilder<'s, '_> {
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
                b.emit_value(ConcatOp::from_sources(ctx, vec![array]), ctx)
            } else {
                array
            })
        };
        if !self.shapes.specialization.sparse_arrays {
            return dense(self, ctx);
        }
        let element = self.array_element(ctx, array)?;
        let is_dense = self.emit_value(perry_mir::ArrayIsDenseOp::new(ctx, array), ctx);
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
        if !self.shapes.specialization.sparse_arrays
            || !expected.is_some_and(|element| perry_mir::admits_undefined(ctx, element))
        {
            return Ok(array);
        }
        let element = self.array_element(ctx, array)?;
        let element = crate::lower::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
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
        self.emit(ArraySetOp::new(ctx, target, index, value), ctx);
        self.jump(ctx, done);
        self.switch_to(hole);
        self.emit(perry_mir::ArrayHoleOp::new(ctx, target, index), ctx);
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
        if !self.shapes.specialization.sparse_arrays {
            return self.element_at(ctx, array, index);
        }
        let op = ArrayGetOp::new(ctx, array, index).to_diag(ctx)?;
        Ok(self.emit_value(op, ctx))
    }

    /// `array[index]` for an `f64` index the caller proved present and in bounds.
    pub(crate) fn element_at(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Value,
    ) -> Result<Value> {
        let at = self.emit_value(
            NumericCastOp::new(
                ctx,
                index,
                IntegerType::get(ctx, 32, Signedness::Signless).into(),
            ),
            ctx,
        );
        let op = ArrayAtOp::new(ctx, array, at).to_diag(ctx)?;
        Ok(self.emit_value(op, ctx))
    }
    pub(crate) fn for_each(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        callback: &'s Expr,
        input: fn(Value) -> Input,
    ) -> Result<Value> {
        let receiver = self.defined(ctx, receiver)?;
        let input = input(receiver);
        let indexed = matches!(input, Input::Array(..));
        let callback = self.prepare_callback(ctx, callback)?;
        self.iterate(ctx, input, |b, ctx, entry, index, _, _| {
            let entry = match entry {
                Item(value) if indexed => Pair(index, value),
                entry => entry,
            };
            b.invoke_callback(
                ctx,
                callback,
                &entry.callback_args(receiver),
                Some(crate::lower::inline::InlineResult::Void),
            )
        })?;
        let ty = void_type(ctx);
        Ok(self.emit_value(UndefinedOp::with_type(ctx, ty), ctx))
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
            b.emit(ArraySetOp::new(ctx, array, position, item), ctx);
            let advanced = b.binary(ctx, BinaryKind::Add, position, one);
            b.store(ctx, write, advanced);
            Ok(())
        })?;
        let retained = self.load(ctx, write)?;
        let integer = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let length = self.emit_value(NumericCastOp::new(ctx, retained, integer), ctx);
        self.emit(ArrayTruncateOp::new(ctx, array, length), ctx);
        Ok(self.compare(ctx, CompareKind::Lt, retained, limit))
    }
}
