//! Statements: bindings, `return`, `throw`, jumps and expression statements.
use super::*;

impl FunctionBuilder<'_> {
    // ── Statements ─────────────────────────────────────────────────────────

    pub(super) fn statements(&mut self, ctx: &mut Context, statements: &[Stmt]) -> Result<()> {
        let mut index = 0;
        while index < statements.len() {
            if let Some(element) = self.destructured_element(&statements[index..]) {
                self.statement(ctx, &element)?;
                index += 2;
                continue;
            }
            self.statement(ctx, &statements[index])?;
            index += 1;
        }
        Ok(())
    }

    /// Array destructuring declares each element as `let e = undefined` and assigns it in
    /// `if (guard) { <iterator pull> } else { e = i < a.length ? a[i] : undefined }`. With a
    /// statically false guard this is `let e = a[i]` (a read that is `undefined` out of
    /// bounds, R8).
    pub(super) fn destructured_element(&self, statements: &[Stmt]) -> Option<Stmt> {
        let [
            Stmt::Let {
                id,
                name,
                ty: HirType::Any,
                init: Some(Expr::Undefined),
                ..
            },
            Stmt::If {
                condition: Expr::LocalGet(guard),
                else_branch: Some(else_branch),
                ..
            },
            ..,
        ] = statements
        else {
            return None;
        };
        let [Stmt::Expr(Expr::LocalSet(target, value))] = else_branch.as_slice() else {
            return None;
        };
        if target != id || !self.false_guards.contains(guard) {
            return None;
        }
        let value = match &**value {
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } if matches!(**else_expr, Expr::Undefined)
                && matches!(**then_expr, Expr::IndexGet { .. }) =>
            {
                (**then_expr).clone()
            }
            other => other.clone(),
        };
        Some(Stmt::Let {
            id: *id,
            name: name.clone(),
            ty: HirType::Any,
            mutable: false,
            init: Some(value),
        })
    }

    pub(super) fn statement(&mut self, ctx: &mut Context, statement: &Stmt) -> Result<()> {
        match statement {
            Stmt::Let {
                id,
                init: Some(guard),
                ..
            } if self.iterator_guard(ctx, guard)? => {
                self.false_guards.insert(*id);
            }
            // The iterator of the protocol arm (`guard ? GetIterator(a) : undefined`).
            Stmt::Let {
                init: Some(Expr::Conditional { condition, .. }),
                ..
            } if matches!(condition.as_ref(), Expr::LocalGet(guard) if self.false_guards.contains(guard)) =>
                {}
            Stmt::If {
                condition: Expr::LocalGet(guard),
                else_branch,
                ..
            } if self.false_guards.contains(guard) => {
                if let Some(else_branch) = else_branch {
                    self.statements(ctx, else_branch)?;
                }
            }
            Stmt::Let { id, ty, init, .. } => {
                // Perry HIR leaves some bindings untyped (`Any`); those take the initializer's
                // type. An explicit type the subset cannot represent is an error, not a fallback.
                // `Array(Any)` is such a binding too (array destructuring's element source).
                let declared = match ty {
                    HirType::Any | HirType::Unknown => None,
                    HirType::Array(element) if matches!(**element, HirType::Any) => None,
                    ty => Some(self.shapes.value_type(ctx, ty)?),
                };
                let init = init
                    .as_ref()
                    .map(|e| self.expression_with(ctx, e, declared))
                    .transpose()?;
                // Inferred bindings preserve absence states and the selected reader mode.
                // HIR's getReader signature assumes the default overload.
                let ty = match (declared, init) {
                    (Some(ty), Some(value))
                        if self.shapes.inferred_bindings.contains(id)
                            && (optional_value(ctx, value.get_type(ctx)) == Some(ty)
                                || (ty.deref(ctx).is::<ReaderType>()
                                    && kind(ctx, value)? == Kind::ByobReader)) =>
                    {
                        value.get_type(ctx)
                    }
                    (Some(ty), _) => ty,
                    (None, Some(value)) => value.get_type(ctx),
                    (None, None) => bail!("an untyped binding needs an initializer"),
                };
                let slot = if self.shapes.global_ids.contains(id) {
                    self.shapes.globals.borrow_mut().insert(*id, ty);
                    self.emit_value(ctx, |ctx| {
                        perry_mir::globals::GlobalOp::new(
                            ctx,
                            format!("global_{id}").try_into().expect("global name"),
                            ty,
                        )
                    })
                } else {
                    self.entry_slot(ctx, ty)
                };
                if self.shapes.global_ids.contains(id) {
                    self.global_slots.insert(slot);
                }
                if let Some(value) = init {
                    let value = self.fit(ctx, value, ty)?;
                    self.store(ctx, slot, value);
                }
                self.locals.insert(*id, slot);
            }
            Stmt::Expr(expr) => {
                self.effect(ctx, expr)?;
            }
            Stmt::Return(value) if !self.callback_returns.is_empty() => {
                self.callback_return(ctx, value.as_ref())?;
            }
            Stmt::Return(value) => {
                let mut adopts_promise = false;
                let mut values = match value {
                    Some(expr) => {
                        let expected = self
                            .results
                            .first()
                            .copied()
                            .context("a void function returns a value")?;
                        let value = self.expression_with(ctx, expr, Some(expected))?;
                        adopts_promise =
                            self.is_async && value.get_type(ctx).deref(ctx).is::<PromiseType>();
                        vec![if adopts_promise {
                            value
                        } else {
                            self.fit(ctx, value, expected)?
                        }]
                    }
                    None => self.implicit_results(ctx),
                };
                self.run_finallies(ctx, |_| true)?;
                if self.terminated(ctx) {
                    self.after_terminator(ctx);
                    return Ok(());
                }
                if adopts_promise {
                    // Returning a promise runs finalizers before adoption; its later rejection
                    // does not enter a catch surrounding the return expression.
                    let handlers = std::mem::take(&mut self.handlers);
                    let await_ = AwaitOp::new(ctx, values[0])?;
                    let value = self.append_value(ctx, &await_);
                    values[0] = self.fit(ctx, value, self.results[0])?;
                    self.handlers = handlers;
                }
                ensure!(
                    values.iter().map(|v| v.get_type(ctx)).collect::<Vec<_>>() == self.results,
                    "return value type differs from the declared result"
                );
                self.emit(ctx, |ctx| ReturnOp::new(ctx, values));
                self.after_terminator(ctx);
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let guards = self.variant_guards.len();
                let test = self.condition(ctx, condition)?;
                let then = self.new_block(ctx, "then");
                let join = self.new_block(ctx, "join");
                let otherwise = if else_branch.is_some() {
                    self.new_block(ctx, "else")
                } else {
                    join
                };
                self.branch(ctx, test, then, otherwise);
                self.switch_to(then);
                self.variant_condition(condition, true);
                self.statements(ctx, then_branch)?;
                let then_reachable = self.reachable(ctx, self.block);
                self.jump(ctx, join);
                self.variant_guards.truncate(guards);
                let mut else_reachable = true;
                if let Some(else_branch) = else_branch {
                    self.switch_to(otherwise);
                    self.variant_condition(condition, false);
                    self.statements(ctx, else_branch)?;
                    else_reachable = self.reachable(ctx, self.block);
                    self.jump(ctx, join);
                }
                self.variant_guards.truncate(guards);
                if !then_reachable {
                    self.variant_condition(condition, false);
                }
                if !else_reachable {
                    self.variant_condition(condition, true);
                }
                self.switch_to(join);
            }
            Stmt::While { condition, body } => {
                self.loop_(ctx, None, Some(condition), None, body, false)?
            }
            Stmt::DoWhile { body, condition } => {
                self.loop_(ctx, None, Some(condition), None, body, true)?
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => self.loop_(
                ctx,
                init.as_deref(),
                condition.as_ref(),
                update.as_ref(),
                body,
                false,
            )?,
            Stmt::Switch {
                discriminant,
                cases,
            } => self.switch(ctx, discriminant, cases)?,
            Stmt::Break | Stmt::Continue => {
                let (index, target) = self
                    .controls
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, control)| match (statement, control) {
                        (
                            Stmt::Break,
                            Control::Loop { break_to, .. } | Control::Switch { break_to },
                        ) => Some((index, *break_to)),
                        (Stmt::Continue, Control::Loop { continue_to, .. }) => {
                            Some((index, *continue_to))
                        }
                        _ => None,
                    })
                    .context("break/continue has no enclosing target")?;
                self.run_finallies(ctx, |f| f.controls > index)?;
                self.jump(ctx, target);
                self.after_terminator(ctx);
            }
            Stmt::Throw(value) => {
                let value = self.expression(ctx, value)?;
                let value = self.defined(ctx, value)?;
                ensure!(
                    matches!(
                        kind(ctx, value)?,
                        Kind::Exception
                            | Kind::Number
                            | Kind::Boolean
                            | Kind::String
                            | Kind::BigInt
                    ),
                    "only Error objects, numbers, bigints, booleans and strings can be thrown"
                );
                let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, value));
                self.propagate(ctx, status);
                self.after_terminator(ctx);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => self.try_(ctx, body, catch.as_ref(), finally.as_deref())?,
            other => bail!(
                "MIR backend does not support statement {} yet",
                statement_name(other)
            ),
        }
        Ok(())
    }

    /// Lower an expression evaluated for its effects; void calls are allowed here.
    pub(super) fn effect(&mut self, ctx: &mut Context, expr: &Expr) -> Result<()> {
        if let Expr::Call { callee, args, .. } = expr {
            self.call(ctx, callee, args)?;
        } else if let Expr::ProcessExit(code) = expr {
            let code = match code {
                Some(code) => {
                    let code = self.expression(ctx, code)?;
                    let code = self.arithmetic(ctx, code);
                    ensure!(
                        kind(ctx, code)? == Kind::Number,
                        "process.exit takes a number"
                    );
                    code
                }
                None => self.number(ctx, 0.0),
            };
            let status = self.emit_value(ctx, |ctx| ExitOp::new(ctx, code));
            self.propagate(ctx, status);
            self.after_terminator(ctx);
        } else {
            self.expression(ctx, expr)?;
        }
        Ok(())
    }

    /// Whether `expr` is array destructuring's guard for the iterator protocol, which is
    /// false for MIR arrays: they are plain arrays, and `Array.prototype[Symbol.iterator]`
    /// cannot be patched. Other sources are an error.
    pub(super) fn iterator_guard(&mut self, ctx: &mut Context, expr: &Expr) -> Result<bool> {
        let source = match expr {
            Expr::ArrayIterationPatched => return Ok(true),
            Expr::NativeMethodCall {
                module,
                object: None,
                method,
                args,
                ..
            } if module == "__perry_runtime" && method == "arrayDestructureNeedsIterator" => {
                let [source] = args.as_slice() else {
                    bail!("arrayDestructureNeedsIterator takes one value");
                };
                source
            }
            _ => return Ok(false),
        };
        let source = self.expression(ctx, source)?;
        ensure!(
            kind(ctx, source)? == Kind::Array,
            "array destructuring is supported for arrays so far"
        );
        Ok(true)
    }
}

fn statement_name(statement: &Stmt) -> String {
    format!("{statement:?}")
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or("?")
        .to_string()
}
