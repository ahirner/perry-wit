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
        self.collect_items(
            ctx,
            Input::Array(array, Holes::Skip),
            |b, ctx, item, current, next| {
                let mapped = b.array_callback(ctx, callback, &[item, current, array])?;
                Ok(match transform {
                    Transform::Map => ArrayOutput::Item(mapped),
                    Transform::FlatMap => ArrayOutput::Items(b.defined(ctx, mapped)?),
                    Transform::Filter => {
                        let keep = b.truthy(ctx, mapped)?;
                        let push = b.new_block(ctx, "array_keep");
                        b.branch(ctx, keep, push, next);
                        b.switch_to(push);
                        ArrayOutput::Item(item)
                    }
                })
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
        self.array_element(ctx, array)?;
        let target = if matches!(mutation, Mutation::Copy) {
            self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![array]))
        } else {
            array
        };
        let limit = self.array_length(ctx, target);
        let one = self.number(ctx, 1.0);
        self.index_loop(ctx, "sort_outer", one, limit, |b, ctx, i, _, _| {
            let key = b.element_at(ctx, target, i)?;
            let insert = b.entry_slot(ctx, i.get_type(ctx));
            b.store(ctx, insert, i);
            let zero = b.number(ctx, 0.0);
            b.index_loop(ctx, "sort_inner", zero, i, |b, ctx, step, _, done| {
                let position = b.binary(ctx, BinaryKind::Sub, i, step);
                let j = b.binary(ctx, BinaryKind::Sub, position, one);
                let candidate = b.element_at(ctx, target, j)?;
                let compare = |b: &mut Self, ctx: &mut Context| {
                    Ok(if let Some(comparator) = comparator {
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
                    })
                };
                let gt = if b.is_optional(ctx, key) {
                    let key_undefined = b.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, key));
                    let bool_ty = key_undefined.get_type(ctx);
                    b.choose(
                        ctx,
                        bool_ty,
                        key_undefined,
                        |b, ctx| Ok(b.boolean(ctx, false)),
                        |b, ctx| {
                            let candidate_undefined =
                                b.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, candidate));
                            b.choose(
                                ctx,
                                bool_ty,
                                candidate_undefined,
                                |b, ctx| Ok(b.boolean(ctx, true)),
                                compare,
                            )
                        },
                    )?
                } else {
                    compare(b, ctx)?
                };
                let shift = b.new_block(ctx, "sort_shift");
                b.branch(ctx, gt, shift, done);
                b.switch_to(shift);
                b.emit(ctx, |ctx| ArraySetOp::new(ctx, target, position, candidate));
                b.store(ctx, insert, j);
                Ok(())
            })?;
            let insert_pos = b.load(ctx, insert)?;
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
        let callback_resolved = match callback {
            Expr::LocalGet(id) => self.closures.get(id).copied().unwrap_or(callback),
            _ => callback,
        };
        let initial = if let Some(init_expr) = initial {
            let expected_acc = if let Expr::Closure { params, .. } = callback_resolved {
                params
                    .first()
                    .map(|p| &p.ty)
                    .filter(|ty| !matches!(ty, HirType::Any | HirType::Unknown))
                    .and_then(|ty| self.shapes.value_type(ctx, ty).ok())
            } else {
                None
            };
            Some(self.expression_with(ctx, init_expr, expected_acc)?)
        } else {
            None
        };
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let (acc_slot, start_step) = if let Some(init_val) = initial {
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
        self.iterate(
            ctx,
            Input::Slice(
                array,
                Span {
                    start: start_step,
                    limit: len,
                    direction,
                },
                Holes::Skip,
            ),
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
        self.array_element(ctx, array)?;
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        if matches!(mutation, Mutation::Copy) {
            self.collect_items(
                ctx,
                Input::Slice(
                    array,
                    Span {
                        start: zero,
                        limit: len,
                        direction: Direction::Reverse,
                    },
                    Holes::Skip,
                ),
                |_, _, item, _, _| Ok(ArrayOutput::Item(item)),
            )
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
        if outer_elem_ty.deref(ctx).is::<ArrayType>() {
            self.collect_items(
                ctx,
                Input::Array(array, Holes::Skip),
                |_, _, array, _, _| Ok(ArrayOutput::Items(array)),
            )
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
        let input = if crate::collections::is_collection_or_view_record(ctx, src_ty)
            || TypedHandle::<ArrayType>::from_handle(src_ty, ctx).is_ok()
        {
            if map_fn.is_none() && src_ty.deref(ctx).is::<ArrayType>() {
                return Ok(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![src_val])));
            }
            Input::Collection(src_val)
        } else if let Ok(rec) = TypedHandle::<RecordType>::from_handle(src_ty, ctx)
            && !rec.deref(ctx).is_tuple()
            && rec.deref(ctx).field("length").is_some()
        {
            let op = FieldGetOp::new(ctx, src_val, "length".to_string())?;
            let raw_len = self.field_read(ctx, &op)?;
            let raw_len = self.arithmetic(ctx, raw_len);
            let len_trunc = self.integer_or_infinity(ctx, raw_len)?;
            let zero = self.number(ctx, 0.0);
            Input::UndefinedCount(self.intrinsic(ctx, Intrinsic::MathMax, vec![len_trunc, zero]))
        } else {
            bail!("Array.from source must be an array, Set, Map, or {{ length }} object");
        };
        let expected_elem = if matches!(input, Input::UndefinedCount(_)) {
            expected
                .and_then(|ty| TypedHandle::<ArrayType>::from_handle(ty, ctx).ok())
                .map(|array| array.deref(ctx).element())
        } else {
            None
        };
        let mapper = map_fn.map(|callback| {
            let zero = self.number(ctx, 0.0);
            let index = self.entry_slot(ctx, zero.get_type(ctx));
            self.store(ctx, index, zero);
            (callback, index)
        });
        self.collect_items(ctx, input, |b, ctx, item, _, _| {
            let item = if let Some((callback, index)) = mapper {
                let ordinal = b.load(ctx, index)?;
                let mapped = b.array_callback(ctx, callback, &[item, ordinal])?;
                let one = b.number(ctx, 1.0);
                let advanced = b.binary(ctx, BinaryKind::Add, ordinal, one);
                b.store(ctx, index, advanced);
                mapped
            } else {
                item
            };
            let item = match expected_elem {
                Some(ty) => b.fit(ctx, item, ty)?,
                None => item,
            };
            Ok(ArrayOutput::Item(item))
        })
    }
}
