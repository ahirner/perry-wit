//! Dense-array methods lowered inline: callbacks of `map`/`filter`, and `indexOf`/`includes`.
use super::*;
use crate::coerce::merge_absence;
use perry_mir::{ArrayAtOp, OptionalType};
/// The frame of an inline array callback being lowered: its `return`s store into `slot` and
/// branch to `done` instead of leaving the enclosing function.
pub(super) struct CallbackReturn {
    done: Ptr<BasicBlock>,
    /// The result slot, from the declared return type or the first typed `return`.
    slot: Option<Value>,
    /// Whether the callback may complete without a value, making its result optional.
    optional: bool,
    /// Blocks ending in `return;` before the slot existed; they store `undefined` afterwards.
    empty_returns: Vec<Ptr<BasicBlock>>,
}

// How a statement list may complete, as a bit set.
const FALLTHROUGH: u8 = 1;
const RETURN: u8 = 2;
const BREAK: u8 = 4;
const CONTINUE: u8 = 8;
const LABELLED: u8 = 16;
// Return-type inference also sees syntactically present returns on unreachable paths.
const EMPTY_RETURN: u8 = 32;

/// The ways `statements` may complete; a callback whose body may fall through returns
/// `undefined` on that path.
fn completions(statements: &[Stmt]) -> u8 {
    let mut result = FALLTHROUGH;
    for statement in statements {
        let next = match statement {
            Stmt::Return(None | Some(Expr::Undefined)) => RETURN | EMPTY_RETURN,
            Stmt::Return(_) | Stmt::Throw(_) => RETURN,
            Stmt::Break => BREAK,
            Stmt::Continue => CONTINUE,
            Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => LABELLED,
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                completions(then_branch)
                    | else_branch
                        .as_ref()
                        .map_or(FALLTHROUGH, |body| completions(body))
            }
            Stmt::Switch { cases, .. } => {
                let mut tail = FALLTHROUGH;
                let mut branches = if cases.iter().any(|case| case.test.is_none()) {
                    0
                } else {
                    FALLTHROUGH
                };
                for case in cases.iter().rev() {
                    let body = completions(&case.body);
                    tail = (body & !FALLTHROUGH) | if body & FALLTHROUGH != 0 { tail } else { 0 };
                    branches |= tail;
                }
                (branches & !BREAK)
                    | if branches & BREAK != 0 {
                        FALLTHROUGH
                    } else {
                        0
                    }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                let body = completions(body);
                (body & (RETURN | EMPTY_RETURN | LABELLED))
                    | if matches!(condition, Expr::Bool(true)) && body & BREAK == 0 {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::For {
                condition, body, ..
            } => {
                let body = completions(body);
                (body & (RETURN | EMPTY_RETURN | LABELLED))
                    | if condition
                        .as_ref()
                        .is_none_or(|c| matches!(c, Expr::Bool(true)))
                        && body & BREAK == 0
                    {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                let body =
                    completions(body) | catch.as_ref().map_or(0, |catch| completions(&catch.body));
                let final_state = finally
                    .as_ref()
                    .map_or(FALLTHROUGH, |body| completions(body));
                (body & EMPTY_RETURN)
                    | (final_state & !FALLTHROUGH)
                    | if final_state & FALLTHROUGH != 0 {
                        body
                    } else {
                        0
                    }
            }
            Stmt::Labeled { body, .. } => {
                FALLTHROUGH | (completions(std::slice::from_ref(body)) & EMPTY_RETURN)
            }
            _ => FALLTHROUGH,
        };
        result = (result & !FALLTHROUGH)
            | if result & FALLTHROUGH != 0 {
                next
            } else {
                next & EMPTY_RETURN
            };
    }
    result
}

pub(super) fn returns_optional(statements: &[Stmt]) -> bool {
    completions(statements) & (FALLTHROUGH | EMPTY_RETURN) != 0
}

impl<'s> FunctionBuilder<'s> {
    /// `return` inside the innermost array callback: store the result and leave for its `done`
    /// block through the callback's own `finally` bodies.
    pub(super) fn callback_return(
        &mut self,
        ctx: &mut Context,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        let Some(mut callback) = self.callback_returns.pop() else {
            bail!("return outside an array callback")
        };
        // The frame is off the stack only while the result is stored: a `finally` body run
        // below may itself `return` from this callback.
        let stored = self.store_callback_result(ctx, &mut callback, expression);
        let done = callback.done;
        self.callback_returns.push(callback);
        stored?;
        self.run_finallies(ctx, |_| true)?;
        self.jump(ctx, done);
        self.after_terminator(ctx);
        Ok(())
    }

    /// Store a returned value in the callback's slot, created by the first typed return.
    /// An untyped `return;` before that is recorded and patched once the slot exists.
    fn store_callback_result(
        &mut self,
        ctx: &mut Context,
        callback: &mut CallbackReturn,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        if callback.slot.is_none() && matches!(expression, None | Some(Expr::Undefined)) {
            callback.empty_returns.push(self.block);
            let finally = self.new_block(ctx, "callback_empty_return");
            self.jump(ctx, finally);
            self.switch_to(finally);
            return Ok(());
        }
        let existing = callback
            .slot
            .map(|slot| self.slot_type(ctx, slot).map(|ty| (slot, ty)))
            .transpose()?;
        let expected = existing.map(|(_, ty)| ty);
        let value = match (expression, expected) {
            (Some(expression), _) => self.expression_with(ctx, expression, expected)?,
            (None, Some(ty)) => self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)),
            (None, None) => {
                let ty = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
            }
        };
        let (slot, ty) = match existing {
            Some(existing) => existing,
            None => {
                let value_type = value.get_type(ctx);
                let ty = if callback.optional {
                    OptionalType::get(ctx, value_type).into()
                } else {
                    value_type
                };
                let slot = self.entry_slot(ctx, ty);
                callback.slot = Some(slot);
                (slot, ty)
            }
        };
        let value = self.fit(ctx, value, ty)?;
        self.store(ctx, slot, value);
        Ok(())
    }

    /// Apply an array callback (a named function or an inline closure) to `arguments`; an
    /// inline closure's body is lowered in place, its `return`s leaving to a `done` block.
    pub(super) fn array_callback(
        &mut self,
        ctx: &mut Context,
        callback: &'s Expr,
        arguments: &[Value],
    ) -> Result<Value> {
        if let Expr::FuncRef(id) = callback {
            let signature = self.signatures.get(id).context("unknown callback")?;
            ensure!(
                !signature.is_async && signature.params.len() <= arguments.len(),
                "array callbacks require synchronous functions with at most three parameters"
            );
            let values = arguments
                .iter()
                .zip(&signature.params)
                .map(|(&v, param)| self.fit(ctx, v, param.carrier))
                .collect::<Result<Vec<_>>>()?;
            let call = CallOp::new(
                ctx,
                signature.name.clone(),
                values,
                signature.carrier_results(),
            );
            self.append(ctx, &call);
            return Ok(if signature.result.is_none() {
                let ty = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
            } else {
                call.get_operation().deref(ctx).get_result(0)
            });
        }
        let Expr::Closure {
            params,
            body,
            is_async,
            is_generator,
            return_type,
            ..
        } = callback
        else {
            bail!("array callbacks require an inline closure or named function")
        };
        ensure!(
            !is_async
                && !is_generator
                && params.len() <= arguments.len()
                && params.iter().all(|p| !p.is_rest),
            "array callbacks must be synchronous with at most three parameters; rest parameters are not supported"
        );
        let saved_locals = self.locals.clone();
        // Guards only grow above this length inside the body; branches truncate back.
        let guards = self.variant_guards.len();
        for (param, &value) in params.iter().zip(arguments) {
            let val_ty = value.get_type(ctx);
            if let Some(default_expr) = &param.default
                && !matches!(default_expr, Expr::Undefined)
                && absence(ctx, val_ty).is_some_and(|a| a.admits_undefined())
            {
                let is_undef = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
                let then_block = self.new_block(ctx, "default_then");
                let else_block = self.new_block(ctx, "default_else");
                let join = self.new_block(ctx, "default_join");
                self.branch(ctx, is_undef, then_block, else_block);

                self.switch_to(else_block);
                let present_val = self.strip_undefined(ctx, value)?;
                let else_end = self.block;
                let present_ty = present_val.get_type(ctx);

                self.switch_to(then_block);
                let def_val = self.expression_with(ctx, default_expr, Some(present_ty))?;
                let then_end = self.block;
                let then_terminated = self.terminated(ctx);

                let target_ty = if !then_terminated {
                    let def_ty = def_val.get_type(ctx);
                    if let Some(states) =
                        merge_absence(absence(ctx, present_ty), absence(ctx, def_ty))
                    {
                        let inner = optional_value(ctx, present_ty)
                            .or_else(|| optional_value(ctx, def_ty))
                            .unwrap_or(present_ty);
                        OptionalType::get_with(ctx, inner, states).into()
                    } else {
                        present_ty
                    }
                } else {
                    present_ty
                };

                let slot = self.entry_slot(ctx, target_ty);
                if !then_terminated {
                    self.switch_to(then_end);
                    let fitted_def = self.fit(ctx, def_val, target_ty)?;
                    self.store(ctx, slot, fitted_def);
                    self.jump(ctx, join);
                }

                self.switch_to(else_end);
                let fitted_present = self.fit(ctx, present_val, target_ty)?;
                self.store(ctx, slot, fitted_present);
                self.jump(ctx, join);

                self.switch_to(join);
                self.locals.insert(param.id, LocalBinding::plain(slot));
            } else {
                let slot = self.entry_slot(ctx, val_ty);
                self.store(ctx, slot, value);
                self.locals.insert(param.id, LocalBinding::plain(slot));
            }
        }
        let finallies = std::mem::take(&mut self.finallies);
        let controls = std::mem::take(&mut self.controls);
        let done = self.new_block(ctx, "callback_done");
        let optional = completions(body) & (FALLTHROUGH | EMPTY_RETURN) != 0;
        let declared = match return_type {
            HirType::Number | HirType::String | HirType::Boolean | HirType::Union(_) => {
                self.shapes.value_type(ctx, return_type).ok()
            }
            _ => None,
        };
        let slot = declared.map(|ty| {
            let ty = if optional {
                OptionalType::get(ctx, ty).into()
            } else {
                ty
            };
            self.entry_slot(ctx, ty)
        });
        self.callback_returns.push(CallbackReturn {
            done,
            slot,
            optional,
            empty_returns: Vec::new(),
        });
        let param_ids: Vec<LocalId> = params.iter().map(|p| p.id).collect();
        let param_defaults: Vec<Option<Expr>> = params.iter().map(|p| p.default.clone()).collect();
        let body =
            crate::builder::strip_synthetic_default_guards(body, &param_ids, &param_defaults);
        let mut result = self.statements(ctx, body);
        if result.is_ok() && self.reachable(ctx, self.block) {
            result = self.callback_return(ctx, None);
        }
        self.locals = saved_locals;
        self.variant_guards.truncate(guards);
        self.finallies = finallies;
        self.controls = controls;
        // Pushes and pops are balanced, so this is the frame pushed above.
        let callback = self
            .callback_returns
            .pop()
            .context("array callback frame")?;
        result?;
        let slot = match callback.slot {
            Some(slot) => Some(slot),
            None if callback.empty_returns.is_empty() => None,
            None => {
                let number = FP64Type::get(ctx).into();
                let ty = OptionalType::get(ctx, number).into();
                Some(self.entry_slot(ctx, ty))
            }
        };
        if let Some(slot) = slot
            && !callback.empty_returns.is_empty()
        {
            let ty = self.slot_type(ctx, slot)?;
            for block in callback.empty_returns {
                let undefined = UndefinedOp::with_type(ctx, ty);
                let store = StoreOp::new(ctx, slot, undefined.get_result(ctx));
                insert_before_terminator(
                    ctx,
                    block,
                    [undefined.get_operation(), store.get_operation()],
                )?;
            }
        }
        self.switch_to(done);
        match slot {
            Some(slot) => self.load(ctx, slot),
            None => {
                let ty = FP64Type::get(ctx).into();
                Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty)))
            }
        }
    }

    /// `array.map(callback)`, or `array.filter(callback)` when `filter`; the callback is
    /// lowered inline once and sees `(item, index, array)`.
    pub(super) fn array_transform(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        filter: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let prepare = self.block;
        self.index_loop(ctx, "array", zero, limit, |b, ctx, current, next, _| {
            // The callback may shrink the array: visit only indices still present.
            let available = b.array_length(ctx, array);
            let present = b.compare(ctx, CompareKind::Lt, current, available);
            let read = b.new_block(ctx, "array_read");
            b.branch(ctx, present, read, next);
            b.switch_to(read);
            let item = b.element_at(ctx, array, current)?;
            let mapped = b.array_callback(ctx, callback, &[item, current, array])?;
            let append = b.block;
            let element = if filter {
                item.get_type(ctx)
            } else {
                mapped.get_type(ctx)
            };
            // The element type is known only now: allocate before the loop already closed.
            let allocation = ArrayNewOp::new(ctx, element, vec![]);
            insert_before_terminator(ctx, prepare, [allocation.get_operation()])?;
            let result = allocation.get_result(ctx);
            b.switch_to(append);
            if filter {
                let keep = b.truthy(ctx, mapped)?;
                let push = b.new_block(ctx, "array_keep");
                b.branch(ctx, keep, push, next);
                b.switch_to(push);
            }
            b.emit(ctx, |ctx| {
                ArrayPushOp::new(ctx, result, if filter { item } else { mapped })
            });
            Ok(result)
        })
    }

    pub(super) fn integer_or_infinity(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        let number = FP64Type::get(ctx).into();
        let zero = self.number(ctx, 0.0);
        let trunc = self.intrinsic(ctx, Intrinsic::MathTrunc, vec![value]);
        let valid = self.compare(ctx, CompareKind::Eq, trunc, trunc);
        self.choose(ctx, number, valid, |_, _| Ok(trunc), |_, _| Ok(zero))
    }

    /// `array.includes(value, from?)` when `includes`, else `array.indexOf(value, from?)`.
    pub(super) fn array_search(
        &mut self,
        ctx: &mut Context,
        array: Value,
        value: &'s Expr,
        from: Option<&'s Expr>,
        includes: bool,
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

    /// `array.find(callback)` or `array.findIndex(callback)` when `index_only`.
    pub(super) fn array_find(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        index_only: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let opt_ty = crate::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
        let miss = if index_only {
            self.number(ctx, -1.0)
        } else {
            self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))
        };
        let result_ty = miss.get_type(ctx);
        let result = self.entry_slot(ctx, result_ty);
        self.store(ctx, result, miss);
        self.index_loop(ctx, "find", zero, limit, |b, ctx, current, next, done| {
            let available = b.array_length(ctx, array);
            let present = b.compare(ctx, CompareKind::Lt, current, available);
            let item = b.choose(
                ctx,
                opt_ty,
                present,
                |b, ctx| {
                    let elem = b.element_at(ctx, array, current)?;
                    b.fit(ctx, elem, opt_ty)
                },
                |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))),
            )?;
            let matched = b.array_callback(ctx, callback, &[item, current, array])?;
            let keep = b.truthy(ctx, matched)?;
            let hit = b.new_block(ctx, "find_hit");
            b.branch(ctx, keep, hit, next);
            b.switch_to(hit);
            let value = if index_only {
                current
            } else {
                b.fit(ctx, item, result_ty)?
            };
            b.store(ctx, result, value);
            b.jump(ctx, done);
            Ok(())
        })?;
        self.load(ctx, result)
    }

    /// `array.some(callback)` or `array.every(callback)` when `every`.
    pub(super) fn array_quantifier(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        every: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let initial = self.boolean(ctx, every);
        let result = self.entry_slot(ctx, initial.get_type(ctx));
        self.store(ctx, result, initial);
        self.index_loop(
            ctx,
            if every { "every" } else { "some" },
            zero,
            limit,
            |b, ctx, current, next, done| {
                let available = b.array_length(ctx, array);
                let present = b.compare(ctx, CompareKind::Lt, current, available);
                let read = b.new_block(ctx, "quantifier_read");
                b.branch(ctx, present, read, next);
                b.switch_to(read);
                let item = b.element_at(ctx, array, current)?;
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

    /// `array.flatMap(callback)`: maps each element to an array and flattens one level.
    pub(super) fn array_flat_map(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let prepare = self.block;
        self.index_loop(ctx, "flat_map", zero, limit, |b, ctx, current, next, _| {
            let available = b.array_length(ctx, array);
            let present = b.compare(ctx, CompareKind::Lt, current, available);
            let read = b.new_block(ctx, "flat_map_read");
            b.branch(ctx, present, read, next);
            b.switch_to(read);
            let item = b.element_at(ctx, array, current)?;
            let mapped = b.array_callback(ctx, callback, &[item, current, array])?;
            let mapped = b.defined(ctx, mapped)?;
            let inner_element = b
                .array_element(ctx, mapped)
                .context("flatMap callback must return an array")?;
            let append = b.block;
            let allocation = ArrayNewOp::new(ctx, inner_element, vec![]);
            insert_before_terminator(ctx, prepare, [allocation.get_operation()])?;
            let result = allocation.get_result(ctx);
            b.switch_to(append);
            let inner_limit = b.array_length(ctx, mapped);
            let inner_zero = b.number(ctx, 0.0);
            b.index_loop(
                ctx,
                "flat_map_inner",
                inner_zero,
                inner_limit,
                |b, ctx, inner_idx, _, _| {
                    let elem = b.element_at(ctx, mapped, inner_idx)?;
                    b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, elem));
                    Ok(())
                },
            )?;
            Ok(result)
        })
    }

    /// `array.sort(comparator?)` or `array.toSorted(comparator?)` when `clone_first`.
    pub(super) fn array_sort(
        &mut self,
        ctx: &mut Context,
        array: Value,
        comparator: Option<&'s Expr>,
        clone_first: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let target = if clone_first {
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
    pub(super) fn array_reduce(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        initial: Option<&'s Expr>,
        from_right: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        self.array_element(ctx, array)?;
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let (acc_slot, start_step) = if let Some(init_expr) = initial {
            let expected_acc = if let Expr::Closure { params, .. } = callback {
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
            let first_idx = if from_right {
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
        self.index_loop(ctx, "reduce", start_step, len, |b, ctx, step, next, _| {
            let current = if from_right {
                let last = b.binary(ctx, BinaryKind::Sub, len, one);
                b.binary(ctx, BinaryKind::Sub, last, step)
            } else {
                step
            };
            let available = b.array_length(ctx, array);
            let present = b.compare(ctx, CompareKind::Lt, current, available);
            let read = b.new_block(ctx, "reduce_read");
            b.branch(ctx, present, read, next);
            b.switch_to(read);
            let item = b.element_at(ctx, array, current)?;
            let acc = b.load(ctx, acc_slot)?;
            let next_acc = b.array_callback(ctx, callback, &[acc, item, current, array])?;
            let fitted = b.fit(ctx, next_acc, acc_ty)?;
            b.store(ctx, acc_slot, fitted);
            Ok(())
        })?;
        self.load(ctx, acc_slot)
    }

    /// `array.join(separator?)`.
    pub(super) fn array_join(
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
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let str_ty = StringType::get(ctx).into();
        let empty = self.string(ctx, "");
        let acc_slot = self.entry_slot(ctx, str_ty);
        self.store(ctx, acc_slot, empty);
        self.index_loop(ctx, "join", zero, len, |b, ctx, step, next, _| {
            let available = b.array_length(ctx, array);
            let present = b.compare(ctx, CompareKind::Lt, step, available);
            let read = b.new_block(ctx, "join_read");
            b.branch(ctx, present, read, next);
            b.switch_to(read);
            let item = b.element_at(ctx, array, step)?;
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
        })?;
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
    pub(super) fn array_slice(
        &mut self,
        ctx: &mut Context,
        array: Value,
        start: Option<&'s Expr>,
        end: Option<&'s Expr>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let elem_ty = self.array_element(ctx, array)?;
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
        let result = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![]));
        self.index_loop(
            ctx,
            "slice",
            norm_start,
            norm_end,
            |b, ctx, step, next, _| {
                let available = b.array_length(ctx, array);
                let present = b.compare(ctx, CompareKind::Lt, step, available);
                let read = b.new_block(ctx, "slice_read");
                b.branch(ctx, present, read, next);
                b.switch_to(read);
                let item = b.element_at(ctx, array, step)?;
                b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, item));
                Ok(())
            },
        )?;
        Ok(result)
    }

    /// `array.reverse()` or `array.toReversed()` when `clone_first`.
    pub(super) fn array_reverse(
        &mut self,
        ctx: &mut Context,
        array: Value,
        clone_first: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let elem_ty = self.array_element(ctx, array)?;
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        if clone_first {
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
    pub(super) fn array_flat(
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

    /// `array.findLast(callback)` or `array.findLastIndex(callback)` when `index_only`.
    pub(super) fn array_find_last(
        &mut self,
        ctx: &mut Context,
        array: Value,
        callback: &'s Expr,
        index_only: bool,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let limit = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let opt_ty = crate::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
        let miss = if index_only {
            self.number(ctx, -1.0)
        } else {
            self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))
        };
        let result_ty = miss.get_type(ctx);
        let result = self.entry_slot(ctx, result_ty);
        self.store(ctx, result, miss);
        self.index_loop(ctx, "find_last", zero, limit, |b, ctx, step, next, done| {
            let last = b.binary(ctx, BinaryKind::Sub, limit, one);
            let current = b.binary(ctx, BinaryKind::Sub, last, step);
            let available = b.array_length(ctx, array);
            let present = b.compare(ctx, CompareKind::Lt, current, available);
            let item = b.choose(
                ctx,
                opt_ty,
                present,
                |b, ctx| {
                    let elem = b.element_at(ctx, array, current)?;
                    b.fit(ctx, elem, opt_ty)
                },
                |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))),
            )?;
            let matched = b.array_callback(ctx, callback, &[item, current, array])?;
            let keep = b.truthy(ctx, matched)?;
            let hit = b.new_block(ctx, "find_last_hit");
            b.branch(ctx, keep, hit, next);
            b.switch_to(hit);
            let value = if index_only {
                current
            } else {
                b.fit(ctx, item, result_ty)?
            };
            b.store(ctx, result, value);
            b.jump(ctx, done);
            Ok(())
        })?;
        self.load(ctx, result)
    }

    /// `array.at(index?)`.
    pub(super) fn array_at(
        &mut self,
        ctx: &mut Context,
        array: Value,
        index: Option<&'s Expr>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let raw_idx = self.number_or(ctx, index, 0.0)?;
        let idx = self.integer_or_infinity(ctx, raw_idx)?;
        let len = self.array_length(ctx, array);
        let zero = self.number(ctx, 0.0);
        let is_neg = self.compare(ctx, CompareKind::Lt, idx, zero);
        let num_ty = FP64Type::get(ctx).into();
        let resolved_idx = self.choose(
            ctx,
            num_ty,
            is_neg,
            |b, ctx| Ok(b.binary(ctx, BinaryKind::Add, len, idx)),
            |_, _| Ok(idx),
        )?;
        let ge_zero = self.compare(ctx, CompareKind::Ge, resolved_idx, zero);
        let lt_len = self.compare(ctx, CompareKind::Lt, resolved_idx, len);
        let in_bounds = self.choose(
            ctx,
            ge_zero.get_type(ctx),
            ge_zero,
            |_, _| Ok(lt_len),
            |b, ctx| Ok(b.boolean(ctx, false)),
        )?;
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
    pub(super) fn array_pop(&mut self, ctx: &mut Context, array: Value) -> Result<Value> {
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
                let u32_ty = IntegerType::get(ctx, 32, Signedness::Unsigned).into();
                let new_len = b.emit_value(ctx, |ctx| NumericCastOp::new(ctx, last_idx, u32_ty));
                b.emit(ctx, |ctx| ArrayTruncateOp::new(ctx, array, new_len));
                b.fit(ctx, item, opt_ty)
            },
            |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))),
        )
    }

    /// `for (i = start; i < limit; i++) body` over `f64` indices. `body` gets `i`, the block
    /// that advances to the next index, and the loop's exit, where lowering continues.
    pub(super) fn index_loop<T>(
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
    pub(super) fn element_at(
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

/// Insert `operations`, in order, before the terminator of an already closed `block`: for
/// values whose type is known only after later code was lowered.
fn insert_before_terminator(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    operations: impl IntoIterator<Item = Ptr<Operation>>,
) -> Result<()> {
    let terminator = block
        .deref(ctx)
        .get_terminator(ctx)
        .context("a closed block ends in a terminator")?;
    for operation in operations {
        operation.insert_before(ctx, terminator);
    }
    Ok(())
}
