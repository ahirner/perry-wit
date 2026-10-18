use super::*;

impl<'s> FunctionBuilder<'s> {
    /// `return` inside the innermost array callback: store the result and leave for its `done`
    /// block through the callback's own `finally` bodies.
    pub(crate) fn callback_return(
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
    pub(crate) fn array_callback(
        &mut self,
        ctx: &mut Context,
        callback: &'s Expr,
        arguments: &[Value],
    ) -> Result<Value> {
        let callback = match callback {
            Expr::LocalGet(id) => self.closures.get(id).copied().unwrap_or(callback),
            _ => callback,
        };
        if let Expr::FuncRef(id) = callback {
            let signature = self.signatures.get(id).context("unknown callback")?;
            ensure!(
                !signature.is_async && signature.params.len() <= arguments.len(),
                "array callbacks require synchronous functions with at most three parameters"
            );
            let values = arguments
                .iter()
                .zip(&signature.params)
                .map(|(&v, param)| self.fit(ctx, v, param.binding.carrier))
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
        let saved_locals = self.locals.scope();
        let saved_closures = self.closures.scope();
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
        let body = hir_compat::strip_synthetic_default_guards(body, params.iter());
        let mut result = self.statements(ctx, body);
        if result.is_ok() && self.reachable(ctx, self.block) {
            result = self.callback_return(ctx, None);
        }
        self.locals.restore(saved_locals);
        self.closures.restore(saved_closures);
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
}
