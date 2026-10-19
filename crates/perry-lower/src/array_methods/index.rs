use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(crate) fn integer_or_infinity(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        let number = FP64Type::get(ctx).into();
        let zero = self.number(ctx, 0.0);
        let trunc = self.intrinsic(ctx, Intrinsic::MathTrunc, vec![value]);
        let valid = self.compare(ctx, CompareKind::Eq, trunc, trunc);
        self.choose(ctx, number, valid, |_, _| Ok(trunc), |_, _| Ok(zero))
    }

    /// `array.join(separator?)`.
    pub(crate) fn array_join(
        &mut self,
        ctx: &mut Context,
        array: Value,
        separator: Option<&'s Expr>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let sep = match separator {
            None | Some(Expr::Undefined) => self.string(ctx, ","),
            Some(sep_expr) => {
                let val = self.expression(ctx, sep_expr)?;
                self.display(ctx, val)?
            }
        };
        let zero = self.number(ctx, 0.0);
        let str_ty = StringType::get(ctx).into();
        let empty = self.string(ctx, "");
        let acc_slot = self.entry_slot(ctx, str_ty);
        self.store(ctx, acc_slot, empty);
        self.iterate(
            ctx,
            Input::Array(array, Holes::Skip),
            |b, ctx, item, step, _, _| {
                let elem_str = if optional_value(ctx, item.get_type(ctx)).is_some() {
                    let undefined = b.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, item));
                    b.choose_string(
                        ctx,
                        undefined,
                        |b, ctx| Ok(b.string(ctx, "")),
                        |b, ctx| {
                            let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, item));
                            b.choose_string(
                                ctx,
                                null,
                                |b, ctx| Ok(b.string(ctx, "")),
                                |b, ctx| {
                                    let defined = b.defined(ctx, item)?;
                                    b.display(ctx, defined)
                                },
                            )
                        },
                    )?
                } else {
                    b.display(ctx, item)?
                };
                let gt_zero = b.compare(ctx, CompareKind::Gt, step, zero);
                let prefix = b.choose(
                    ctx,
                    str_ty,
                    gt_zero,
                    |b, ctx| {
                        let prev = b.load(ctx, acc_slot)?;
                        Ok(b.emit_value(ctx, |ctx| ConcatOp::new(ctx, prev, sep)))
                    },
                    |b, ctx| b.load(ctx, acc_slot),
                )?;
                let next_str = b.emit_value(ctx, |ctx| ConcatOp::new(ctx, prefix, elem_str));
                b.store(ctx, acc_slot, next_str);
                Ok(())
            },
        )?;
        self.load(ctx, acc_slot)
    }

    fn normalize_slice_bound(
        &mut self,
        ctx: &mut Context,
        bound: Value,
        len: Value,
    ) -> Result<Value> {
        let trunc = self.integer_or_infinity(ctx, bound)?;
        let zero = self.number(ctx, 0.0);
        let is_neg = self.compare(ctx, CompareKind::Lt, trunc, zero);
        let num_ty = FP64Type::get(ctx).into();
        self.choose(
            ctx,
            num_ty,
            is_neg,
            |b, ctx| {
                let sum = b.binary(ctx, BinaryKind::Add, len, trunc);
                Ok(b.intrinsic(ctx, Intrinsic::MathMax, vec![sum, zero]))
            },
            |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::MathMin, vec![trunc, len])),
        )
    }

    /// `array.slice(start?, end?)`.
    pub(crate) fn array_slice(
        &mut self,
        ctx: &mut Context,
        array: Value,
        start: Option<&'s Expr>,
        end: Option<&'s Expr>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let len = self.array_length(ctx, array);
        let norm_start = match start {
            None | Some(Expr::Undefined) => self.number(ctx, 0.0),
            Some(expr) => {
                let s = self.number_operand(ctx, expr)?;
                self.normalize_slice_bound(ctx, s, len)?
            }
        };
        let norm_end = match end {
            None | Some(Expr::Undefined) => len,
            Some(expr) => {
                let e = self.number_operand(ctx, expr)?;
                self.normalize_slice_bound(ctx, e, len)?
            }
        };
        self.collect_items(
            ctx,
            Input::Slice(
                array,
                Span {
                    start: norm_start,
                    limit: norm_end,
                    direction: Direction::Forward,
                },
                Holes::Skip,
            ),
            |_, _, item, _, _| Ok(ArrayOutput::Item(item)),
        )
    }

    pub(super) fn resolve_relative_index(
        &mut self,
        ctx: &mut Context,
        raw_idx: Value,
        len: Value,
    ) -> Result<Value> {
        let idx = self.integer_or_infinity(ctx, raw_idx)?;
        let zero = self.number(ctx, 0.0);
        let is_neg = self.compare(ctx, CompareKind::Lt, idx, zero);
        let num_ty = FP64Type::get(ctx).into();
        self.choose(
            ctx,
            num_ty,
            is_neg,
            |b, ctx| Ok(b.binary(ctx, BinaryKind::Add, len, idx)),
            |_, _| Ok(idx),
        )
    }

    fn resolve_checked_index(
        &mut self,
        ctx: &mut Context,
        raw_idx: Value,
        len: Value,
    ) -> Result<(Value, Value)> {
        let resolved_idx = self.resolve_relative_index(ctx, raw_idx, len)?;
        let zero = self.number(ctx, 0.0);
        let ge_zero = self.compare(ctx, CompareKind::Ge, resolved_idx, zero);
        let lt_len = self.compare(ctx, CompareKind::Lt, resolved_idx, len);
        let in_bounds = self.choose(
            ctx,
            ge_zero.get_type(ctx),
            ge_zero,
            |_, _| Ok(lt_len),
            |b, ctx| Ok(b.boolean(ctx, false)),
        )?;
        Ok((resolved_idx, in_bounds))
    }

    /// `array.at(index?)`.
    pub(crate) fn array_at(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Option<&'s Expr>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let raw_idx = self.number_or(ctx, index, 0.0)?;
        let len = self.array_length(ctx, array);
        let (resolved_idx, in_bounds) = self.resolve_checked_index(ctx, raw_idx, len)?;
        let opt_ty = crate::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
        self.choose(
            ctx,
            opt_ty,
            in_bounds,
            |b, ctx| {
                let item = b.element_at(ctx, array, resolved_idx)?;
                b.fit(ctx, item, opt_ty)
            },
            |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))),
        )
    }

    /// `array.pop()`.
    pub(crate) fn array_pop(&mut self, ctx: &mut Context, array: Value) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let opt_ty = crate::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let has_elems = self.compare(ctx, CompareKind::Gt, len, zero);
        self.choose(
            ctx,
            opt_ty,
            has_elems,
            |b, ctx| {
                let one = b.number(ctx, 1.0);
                let last_idx = b.binary(ctx, BinaryKind::Sub, len, one);
                let item = b.element_at(ctx, array, last_idx)?;
                let u32_ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
                let new_len = b.emit_value(ctx, |ctx| NumericCastOp::new(ctx, last_idx, u32_ty));
                b.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, array, new_len));
                b.fit(ctx, item, opt_ty)
            },
            |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))),
        )
    }

    /// `array.with(index, value)`.
    pub(crate) fn array_with(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: &'s Expr,
        value: &'s Expr,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let elem_ty = self.array_element(ctx, array)?;
        let raw_idx = self.number_operand(ctx, index)?;
        let val = self.expression_with(ctx, value, Some(elem_ty))?;
        let val = self.fit(ctx, val, elem_ty)?;
        let len = self.array_length(ctx, array);
        let (resolved_idx, in_bounds) = self.resolve_checked_index(ctx, raw_idx, len)?;
        let ok_b = self.new_block(ctx, "with_ok");
        let err_b = self.new_block(ctx, "with_err");
        self.branch(ctx, in_bounds, ok_b, err_b);
        self.switch_to(err_b);
        self.raise_range_error(ctx, "Invalid index for Array.prototype.with");
        self.switch_to(ok_b);
        let cloned = self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![array]));
        self.emit(ctx, |ctx| ArraySetOp::new(ctx, cloned, resolved_idx, val));
        Ok(cloned)
    }
}
