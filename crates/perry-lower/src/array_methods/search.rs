use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(crate) fn array_search(
        &mut self,
        ctx: &mut Context,
        array: Value,
        value: &'s Expr,
        from: Option<&'s Expr>,
        result: SearchResult,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let needle = self.expression_with(ctx, value, Some(element))?;
        let needle = self.fit(ctx, needle, element)?;
        let number = FP64Type::get(ctx).into();
        let zero = self.number(ctx, 0.0);
        let start = match from {
            Some(from) => self.number_operand(ctx, from)?,
            None => zero,
        };
        let limit = self.array_length(ctx, array);
        let start = self.integer_or_infinity(ctx, start)?;
        let negative = self.compare(ctx, CompareKind::Lt, start, zero);
        let start = self.choose(
            ctx,
            number,
            negative,
            |b, ctx| Ok(b.binary(ctx, BinaryKind::Add, limit, start)),
            |_, _| Ok(start),
        )?;
        let below = self.compare(ctx, CompareKind::Lt, start, zero);
        let start = self.choose(ctx, number, below, |_, _| Ok(zero), |_, _| Ok(start))?;
        let includes = result == SearchResult::Includes;
        let miss = if includes {
            self.boolean(ctx, false)
        } else {
            self.number(ctx, -1.0)
        };
        let result = self.entry_slot(ctx, miss.get_type(ctx));
        self.store(ctx, result, miss);
        self.index_loop(
            ctx,
            "search",
            start,
            limit,
            |b, ctx, current, next, done| {
                let item = b.element_at(ctx, array, current)?;
                let equal = b.compare(ctx, CompareKind::Eq, item, needle);
                let found = b.new_block(ctx, "search_found");
                if includes {
                    // `includes` uses SameValueZero: NaN finds NaN.
                    let nan = b.new_block(ctx, "search_nan");
                    b.branch(ctx, equal, found, nan);
                    b.switch_to(nan);
                    let item_nan = b.compare(ctx, CompareKind::Ne, item, item);
                    let needle_nan = b.compare(ctx, CompareKind::Ne, needle, needle);
                    let both = b.choose(
                        ctx,
                        item_nan.get_type(ctx),
                        item_nan,
                        |_, _| Ok(needle_nan),
                        |_, _| Ok(item_nan),
                    )?;
                    b.branch(ctx, both, found, next);
                } else {
                    b.branch(ctx, equal, found, next);
                }
                b.switch_to(found);
                let value = if includes {
                    b.boolean(ctx, true)
                } else {
                    current
                };
                b.store(ctx, result, value);
                b.jump(ctx, done);
                Ok(())
            },
        )?;
        self.load(ctx, result)
    }

    pub(crate) fn array_find(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        result: FindResult,
        direction: Direction,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let opt_ty = crate::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
        let miss = if matches!(result, FindResult::Index) {
            self.number(ctx, -1.0)
        } else {
            self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))
        };
        let result_ty = miss.get_type(ctx);
        let slot = self.entry_slot(ctx, result_ty);
        self.store(ctx, slot, miss);
        self.dense_loop(
            ctx,
            ArrayIteration {
                array,
                start: zero,
                limit,
                direction,
                holes: Holes::Visit(opt_ty),
            },
            |b, ctx, item, current, next, done| {
                let matched = b.array_callback(ctx, callback, &[item, current, array])?;
                let keep = b.truthy(ctx, matched)?;
                let hit = b.new_block(ctx, "find_hit");
                b.branch(ctx, keep, hit, next);
                b.switch_to(hit);
                let value = if matches!(result, FindResult::Index) {
                    current
                } else {
                    b.fit(ctx, item, result_ty)?
                };
                b.store(ctx, slot, value);
                b.jump(ctx, done);
                Ok(())
            },
        )?;
        self.load(ctx, slot)
    }

    pub(crate) fn array_quantifier(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        quantifier: Quantifier,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let every = matches!(quantifier, Quantifier::Every);
        let initial = self.boolean(ctx, every);
        let result = self.entry_slot(ctx, initial.get_type(ctx));
        self.store(ctx, result, initial);
        self.dense_loop(
            ctx,
            ArrayIteration {
                array,
                start: zero,
                limit,
                direction: Direction::Forward,
                holes: Holes::Skip,
            },
            |b, ctx, item, current, next, done| {
                let tested = b.array_callback(ctx, callback, &[item, current, array])?;
                let truthy = b.truthy(ctx, tested)?;
                let stop = b.new_block(ctx, "quantifier_stop");
                if every {
                    b.branch(ctx, truthy, next, stop);
                } else {
                    b.branch(ctx, truthy, stop, next);
                }
                b.switch_to(stop);
                let outcome = b.boolean(ctx, !every);
                b.store(ctx, result, outcome);
                b.jump(ctx, done);
                Ok(())
            },
        )?;
        self.load(ctx, result)
    }

    /// `array.lastIndexOf(needle, fromIndex?)`.
    pub(crate) fn array_last_index_of(
        &mut self,
        ctx: &mut Context,
        array: Value,
        needle: &'s Expr,
        from: Option<&'s Expr>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let needle = self.expression(ctx, needle)?;
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let neg_one = self.number(ctx, -1.0);
        let num_ty = FP64Type::get(ctx).into();
        let last_idx = self.binary(ctx, BinaryKind::Sub, len, one);
        let start = match from {
            None | Some(Expr::Undefined) => last_idx,
            Some(from_expr) => {
                let raw_from = self.number_operand(ctx, from_expr)?;
                let from_val = self.integer_or_infinity(ctx, raw_from)?;
                let is_neg = self.compare(ctx, CompareKind::Lt, from_val, zero);
                self.choose(
                    ctx,
                    num_ty,
                    is_neg,
                    |b, ctx| Ok(b.binary(ctx, BinaryKind::Add, len, from_val)),
                    |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::MathMin, vec![from_val, last_idx])),
                )?
            }
        };
        let count_raw = self.binary(ctx, BinaryKind::Add, start, one);
        let count = self.intrinsic(ctx, Intrinsic::MathMax, vec![count_raw, zero]);
        let result = self.entry_slot(ctx, num_ty);
        self.store(ctx, result, neg_one);
        self.index_loop(
            ctx,
            "last_index_of",
            zero,
            count,
            |b, ctx, step, next, done| {
                let current = b.binary(ctx, BinaryKind::Sub, start, step);
                let available = b.array_length(ctx, array);
                let present = b.compare(ctx, CompareKind::Lt, current, available);
                let read = b.new_block(ctx, "last_index_read");
                b.branch(ctx, present, read, next);
                b.switch_to(read);
                let item = b.element_at(ctx, array, current)?;
                let matched = b.compare(ctx, CompareKind::Eq, item, needle);
                let hit = b.new_block(ctx, "last_index_hit");
                b.branch(ctx, matched, hit, next);
                b.switch_to(hit);
                b.store(ctx, result, current);
                b.jump(ctx, done);
                Ok(())
            },
        )?;
        self.load(ctx, result)
    }
}
