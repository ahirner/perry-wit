use super::*;

impl<'s> FunctionBuilder<'s> {
    /// `array.map(callback)`, or `array.filter(callback)` when `filter`; the callback is
    /// lowered inline once and sees `(item, index, array)`.
    pub(crate) fn array_transform(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        transform: Transform,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let start = self.number(ctx, 0.0);
        let prepare = self.block;
        self.dense_loop(
            ctx,
            ArrayIteration {
                array,
                start,
                limit,
                direction: Direction::Forward,
                holes: Holes::Skip,
            },
            |b, ctx, item, current, next, _| {
                let mapped = b.array_callback(ctx, callback, &[item, current, array])?;
                let (mapped, element) = match transform {
                    Transform::Map => (mapped, mapped.get_type(ctx)),
                    Transform::Filter => (mapped, item.get_type(ctx)),
                    Transform::FlatMap => {
                        let mapped = b.defined(ctx, mapped)?;
                        let element = b
                            .array_element(ctx, mapped)
                            .context("flatMap callback must return an array")?;
                        (mapped, element)
                    }
                };
                let allocation = ArrayNewOp::new(ctx, element, vec![]);
                insert_before_terminator(ctx, prepare, [allocation.get_operation()])?;
                let result = allocation.get_result(ctx);
                match transform {
                    Transform::Filter => {
                        let keep = b.truthy(ctx, mapped)?;
                        let push = b.new_block(ctx, "array_keep");
                        b.branch(ctx, keep, push, next);
                        b.switch_to(push);
                        b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, item));
                    }
                    Transform::Map => {
                        b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, mapped));
                    }
                    Transform::FlatMap => {
                        let limit = b.array_length(ctx, mapped);
                        let start = b.number(ctx, 0.0);
                        b.index_loop(
                            ctx,
                            "flat_map_inner",
                            start,
                            limit,
                            |b, ctx, index, _, _| {
                                let element = b.element_at(ctx, mapped, index)?;
                                b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, element));
                                Ok(())
                            },
                        )?;
                    }
                }
                Ok(result)
            },
        )
    }

    /// `array.sort(comparator?)` or `array.toSorted(comparator?)` when copied.
    pub(crate) fn array_sort(
        &mut self,
        ctx: &mut Context,
        array: Value,
        comparator: Option<&'s Expr>,
        mutation: Mutation,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let target = if matches!(mutation, Mutation::Copy) {
            let copy = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
            let limit = self.array_length(ctx, array);
            let zero = self.number(ctx, 0.0);
            self.index_loop(ctx, "sort_clone", zero, limit, |b, ctx, idx, _, _| {
                let item = b.element_at(ctx, array, idx)?;
                b.emit(ctx, |ctx| ArrayPushOp::new(ctx, copy, item));
                Ok(())
            })?;
            copy
        } else {
            array
        };
        let limit = self.array_length(ctx, target);
        let one = self.number(ctx, 1.0);
        self.index_loop(ctx, "sort_outer", one, limit, |b, ctx, i, _, _| {
            let key = b.element_at(ctx, target, i)?;
            let j_slot = b.entry_slot(ctx, i.get_type(ctx));
            let one = b.number(ctx, 1.0);
            let zero = b.number(ctx, 0.0);
            let j_init = b.binary(ctx, BinaryKind::Sub, i, one);
            b.store(ctx, j_slot, j_init);
            let inner_header = b.new_block(ctx, "sort_inner_header");
            let inner_cmp = b.new_block(ctx, "sort_inner_cmp");
            let inner_shift = b.new_block(ctx, "sort_inner_shift");
            let inner_done = b.new_block(ctx, "sort_inner_done");
            b.jump(ctx, inner_header);
            b.switch_to(inner_header);
            let j = b.load(ctx, j_slot)?;
            let ge_zero = b.compare(ctx, CompareKind::Ge, j, zero);
            b.branch(ctx, ge_zero, inner_cmp, inner_done);
            b.switch_to(inner_cmp);
            let candidate = b.element_at(ctx, target, j)?;
            let gt = if let Some(comparator) = comparator {
                let cmp = b.array_callback(ctx, comparator, &[candidate, key])?;
                let cmp = b.arithmetic(ctx, cmp);
                ensure!(
                    kind(ctx, cmp)? == Kind::Number,
                    "sort comparator must return a number"
                );
                b.compare(ctx, CompareKind::Gt, cmp, zero)
            } else {
                let cand_str = b.display(ctx, candidate)?;
                let key_str = b.display(ctx, key)?;
                b.compare(ctx, CompareKind::Gt, cand_str, key_str)
            };
            b.branch(ctx, gt, inner_shift, inner_done);
            b.switch_to(inner_shift);
            let j_plus_one = b.binary(ctx, BinaryKind::Add, j, one);
            b.emit(ctx, |ctx| {
                ArraySetOp::new(ctx, target, j_plus_one, candidate)
            });
            let j_minus_one = b.binary(ctx, BinaryKind::Sub, j, one);
            b.store(ctx, j_slot, j_minus_one);
            b.jump(ctx, inner_header);
            b.switch_to(inner_done);
            let j_final = b.load(ctx, j_slot)?;
            let insert_pos = b.binary(ctx, BinaryKind::Add, j_final, one);
            b.emit(ctx, |ctx| ArraySetOp::new(ctx, target, insert_pos, key));
            Ok(())
        })?;
        Ok(target)
    }

    /// `array.reduce(callback, initial?)` or `array.reduceRight(callback, initial?)`.
    pub(crate) fn array_reduce(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        initial: Option<&'s Expr>,
        direction: Direction,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let callback_resolved = match callback {
            Expr::LocalGet(id) => self.closures.get(id).copied().unwrap_or(callback),
            _ => callback,
        };
        let (acc_slot, start_step) = if let Some(init_expr) = initial {
            let expected_acc = if let Expr::Closure { params, .. } = callback_resolved {
                params
                    .first()
                    .map(|p| &p.ty)
                    .filter(|ty| !matches!(ty, HirType::Any | HirType::Unknown))
                    .and_then(|ty| self.shapes.value_type(ctx, ty).ok())
            } else {
                None
            };
            let init_val = self.expression_with(ctx, init_expr, expected_acc)?;
            let slot = self.entry_slot(ctx, init_val.get_type(ctx));
            self.store(ctx, slot, init_val);
            (slot, zero)
        } else {
            let non_empty = self.compare(ctx, CompareKind::Gt, len, zero);
            let ok_block = self.new_block(ctx, "reduce_non_empty");
            let err_block = self.new_block(ctx, "reduce_empty_err");
            self.branch(ctx, non_empty, ok_block, err_block);
            self.switch_to(err_block);
            let msg = self.string(ctx, "Reduce of empty array with no initial value");
            let err = self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, ErrorKind::TypeError, msg));
            let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, err));
            self.propagate(ctx, status);
            self.after_terminator(ctx);
            self.switch_to(ok_block);
            let first_idx = if matches!(direction, Direction::Reverse) {
                self.binary(ctx, BinaryKind::Sub, len, one)
            } else {
                zero
            };
            let first_elem = self.element_at(ctx, array, first_idx)?;
            let slot = self.entry_slot(ctx, first_elem.get_type(ctx));
            self.store(ctx, slot, first_elem);
            (slot, one)
        };
        let acc_ty = self.slot_type(ctx, acc_slot)?;
        self.dense_loop(
            ctx,
            ArrayIteration {
                array,
                start: start_step,
                limit: len,
                direction,
                holes: Holes::Skip,
            },
            |b, ctx, item, current, _, _| {
                let acc = b.load(ctx, acc_slot)?;
                let next_acc = b.array_callback(ctx, callback, &[acc, item, current, array])?;
                let fitted = b.fit(ctx, next_acc, acc_ty)?;
                b.store(ctx, acc_slot, fitted);
                Ok(())
            },
        )?;
        self.load(ctx, acc_slot)
    }

    /// `array.reverse()` or `array.toReversed()` when copied.
    pub(crate) fn array_reverse(
        &mut self,
        ctx: &mut Context,
        array: Value,
        mutation: Mutation,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let elem_ty = self.array_element(ctx, array)?;
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        if matches!(mutation, Mutation::Copy) {
            let result = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![]));
            self.index_loop(ctx, "to_reversed", zero, len, |b, ctx, step, next, _| {
                let last = b.binary(ctx, BinaryKind::Sub, len, one);
                let idx = b.binary(ctx, BinaryKind::Sub, last, step);
                let available = b.array_length(ctx, array);
                let present = b.compare(ctx, CompareKind::Lt, idx, available);
                let read = b.new_block(ctx, "to_reversed_read");
                b.branch(ctx, present, read, next);
                b.switch_to(read);
                let item = b.element_at(ctx, array, idx)?;
                b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, item));
                Ok(())
            })?;
            Ok(result)
        } else {
            let two = self.number(ctx, 2.0);
            let half_raw = self.binary(ctx, BinaryKind::Div, len, two);
            let half = self.intrinsic(ctx, Intrinsic::MathFloor, vec![half_raw]);
            self.index_loop(ctx, "reverse", zero, half, |b, ctx, step, _, _| {
                let last = b.binary(ctx, BinaryKind::Sub, len, one);
                let other_idx = b.binary(ctx, BinaryKind::Sub, last, step);
                let left = b.element_at(ctx, array, step)?;
                let right = b.element_at(ctx, array, other_idx)?;
                b.emit(ctx, |ctx| ArraySetOp::new(ctx, array, step, right));
                b.emit(ctx, |ctx| ArraySetOp::new(ctx, array, other_idx, left));
                Ok(())
            })?;
            Ok(array)
        }
    }

    /// `array.flat(depth?)`.
    pub(crate) fn array_flat(
        &mut self,
        ctx: &mut Context,
        array: Value,
        depth: Option<&'s Expr>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let outer_elem_ty = self.array_element(ctx, array)?;
        let depth_num = match depth {
            None | Some(Expr::Undefined) => 1,
            Some(Expr::Number(n)) if *n <= 0.0 => 0,
            Some(Expr::Number(n)) if (*n as i32) == 1 => 1,
            Some(Expr::Integer(n)) if *n <= 0 => 0,
            Some(Expr::Integer(1)) => 1,
            _ => bail!("array.flat requires a constant depth of 0 or 1 in typed arrays"),
        };
        if depth_num == 0 {
            return Ok(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![array])));
        }
        if let Ok(inner_arr_ty) = TypedHandle::<ArrayType>::from_handle(outer_elem_ty, ctx) {
            let inner_elem_ty = inner_arr_ty.deref(ctx).element();
            let result = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, inner_elem_ty, vec![]));
            let len = self.array_length(ctx, array);
            let zero = self.number(ctx, 0.0);
            self.index_loop(ctx, "flat", zero, len, |b, ctx, step, next, _| {
                let available = b.array_length(ctx, array);
                let present = b.compare(ctx, CompareKind::Lt, step, available);
                let read = b.new_block(ctx, "flat_read");
                b.branch(ctx, present, read, next);
                b.switch_to(read);
                let sub = b.element_at(ctx, array, step)?;
                b.emit(ctx, |ctx| perry_mir::ArrayExtendOp::new(ctx, result, sub));
                Ok(())
            })?;
            Ok(result)
        } else {
            Ok(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![array])))
        }
    }

    /// `Array.from(source, mapFn?)`.
    pub(crate) fn array_from(
        &mut self,
        ctx: &mut Context,
        source: &'s Expr,
        map_fn: Option<&'s Expr>,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let map_fn = map_fn.filter(|f| !matches!(f, Expr::Undefined));
        let src_val = self.expression(ctx, source)?;
        let src_val = self.defined(ctx, src_val)?;
        let src_ty = src_val.get_type(ctx);
        let arr_val = if crate::collections::is_collection_or_view_record(ctx, src_ty)
            || crate::collections::set_entry_type(ctx, src_ty).is_some()
            || crate::collections::map_entry_types(ctx, src_ty).is_some()
        {
            self.materialize_collection_to_array(ctx, src_val)?
        } else if TypedHandle::<ArrayType>::from_handle(src_ty, ctx).is_ok() {
            if map_fn.is_none() {
                return Ok(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![src_val])));
            }
            src_val
        } else if let Ok(rec) = TypedHandle::<RecordType>::from_handle(src_ty, ctx)
            && !rec.deref(ctx).is_tuple()
            && rec.deref(ctx).field("length").is_some()
        {
            let op = FieldGetOp::new(ctx, src_val, "length".to_string())?;
            let raw_len = self.field_read(ctx, &op)?;
            let raw_len = self.arithmetic(ctx, raw_len);
            let len_trunc = self.integer_or_infinity(ctx, raw_len)?;
            let zero = self.number(ctx, 0.0);
            let len = self.intrinsic(ctx, Intrinsic::MathMax, vec![len_trunc, zero]);
            let expected_elem = expected
                .and_then(|ty| TypedHandle::<ArrayType>::from_handle(ty, ctx).ok())
                .map(|a| a.deref(ctx).element());
            let prepare = self.block;
            return self.index_loop(ctx, "array_from_len", zero, len, |b, ctx, idx, _, _| {
                // Array-likes read as `undefined` elements.
                let void = crate::coerce::void_type(ctx);
                let undef = b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, void));
                let item = if let Some(cb) = map_fn {
                    let mapped = b.array_callback(ctx, cb, &[undef, idx])?;
                    match expected_elem {
                        Some(elem_ty) => b.fit(ctx, mapped, elem_ty)?,
                        None => mapped,
                    }
                } else {
                    match expected_elem {
                        Some(elem_ty) => b.fit(ctx, undef, elem_ty)?,
                        None => undef,
                    }
                };
                let append = b.block;
                let elem_ty = expected_elem.unwrap_or_else(|| item.get_type(ctx));
                let allocation = ArrayNewOp::new(ctx, elem_ty, vec![]);
                insert_before_terminator(ctx, prepare, [allocation.get_operation()])?;
                let result = allocation.get_result(ctx);
                b.switch_to(append);
                b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, item));
                Ok(result)
            });
        } else {
            bail!("Array.from source must be an array, Set, Map, or {{ length }} object");
        };
        let Some(cb) = map_fn else {
            return Ok(arr_val);
        };
        self.array_transform(ctx, arr_val, cb, Transform::Map)
    }
}
