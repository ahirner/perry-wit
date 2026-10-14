//! Statements: bindings, `return`, `throw`, jumps and expression statements.
use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn statements(&mut self, ctx: &mut Context, statements: &'s [Stmt]) -> Result<()> {
        let mut rest = statements;
        while let Some((first, tail)) = rest.split_first() {
            rest = match self.destructured_element(rest) {
                Some((id, value, after)) => {
                    self.binding(ctx, id, &HirType::Any, Some(value))?;
                    after
                }
                None => {
                    self.statement(ctx, first).map_err(|error| match first {
                        Stmt::Let { id, .. } => self.shapes.annotate_local(error, *id),
                        _ => error,
                    })?;
                    tail
                }
            };
        }
        Ok(())
    }

    /// Array destructuring declares each element as `let e = undefined` and assigns it in
    /// `if (guard) { <iterator pull> } else { e = i < a.length ? a[i] : undefined }`. With a
    /// statically false guard this is `let e = a[i]` (a read that is `undefined` out of
    /// bounds, R8): the element, its initializer and the statements after the pair.
    fn destructured_element(
        &self,
        statements: &'s [Stmt],
    ) -> Option<(LocalId, &'s Expr, &'s [Stmt])> {
        let [
            Stmt::Let {
                id,
                ty: HirType::Any,
                init: Some(Expr::Undefined),
                ..
            },
            Stmt::If {
                condition: Expr::LocalGet(guard),
                else_branch: Some(else_branch),
                ..
            },
            after @ ..,
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
                &**then_expr
            }
            other => other,
        };
        Some((*id, value, after))
    }

    pub(super) fn statement(&mut self, ctx: &mut Context, statement: &'s Stmt) -> Result<()> {
        match statement {
            Stmt::If {
                condition: Expr::LocalGet(guard),
                else_branch,
                ..
            } if self.false_guards.contains(guard) => {
                if let Some(else_branch) = else_branch {
                    self.statements(ctx, else_branch)?;
                }
            }
            Stmt::Let { id, ty, init, .. } => self
                .binding(ctx, *id, ty, init.as_ref())
                .map_err(|error| self.shapes.annotate_local(error, *id))?,
            Stmt::Expr(expr) => {
                self.effect(ctx, expr)?;
            }
            Stmt::Return(value) if !self.callback_returns.is_empty() => {
                self.callback_return(ctx, value.as_ref())?;
            }
            Stmt::Return(value) => {
                // An async function adopts a returned promise (and its result type) after
                // the finalizers ran.
                let (mut values, adopted) = match value {
                    Some(expr) => {
                        let result = self.result.context("a void function returns a value")?;
                        let value =
                            self.expression_with(ctx, expr, Some(result.expected_type(ctx)))?;
                        if self.is_async && value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                            (Vec::with_capacity(1), Some((value, result.carrier)))
                        } else {
                            (vec![self.fit_binding(ctx, value, result)?], None)
                        }
                    }
                    None => (self.implicit_results(ctx), None),
                };
                self.run_finallies(ctx, |_| true)?;
                if self.terminated(ctx) {
                    self.after_terminator(ctx);
                    return Ok(());
                }
                if let Some((promise, expected)) = adopted {
                    // Returning a promise runs finalizers before adoption; its later rejection
                    // does not enter a catch surrounding the return expression.
                    let handlers = std::mem::take(&mut self.handlers);
                    let await_ = AwaitOp::new(ctx, promise)?;
                    let value = self.append_value(ctx, &await_);
                    values.push(self.fit(ctx, value, expected)?);
                    self.handlers = handlers;
                }
                ensure!(
                    values
                        .iter()
                        .map(|v| v.get_type(ctx))
                        .eq(self.result.iter().map(|r| r.carrier)),
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
                ensure!(
                    matches!(
                        kind(ctx, value).or_else(|_| optional_value(ctx, value.get_type(ctx))
                            .map(|_| Kind::Number)
                            .context("unsupported thrown value"))?,
                        Kind::Exception
                            | Kind::Json
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
            // Perry HIR exposes no variant names, only `Debug`: show the whole node.
            other => bail!("MIR backend does not support statement {other:?} yet"),
        }
        Ok(())
    }

    /// `let id: ty = init`. Array destructuring's iterator guard and the iterator of its
    /// protocol arm (`guard ? GetIterator(a) : undefined`) bind nothing when the guard is
    /// statically false.
    fn binding(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        ty: &HirType,
        init: Option<&'s Expr>,
    ) -> Result<()> {
        if let Some(guard) = init
            && self.iterator_guard(ctx, guard)?
        {
            self.false_guards.insert(id);
            return Ok(());
        }
        if let Some(Expr::Conditional { condition, .. }) = init
            && matches!(condition.as_ref(), Expr::LocalGet(guard) if self.false_guards.contains(guard))
        {
            return Ok(());
        }
        // Perry HIR leaves some bindings untyped (`Any`); those take the initializer's
        // type. An explicit type the subset cannot represent is an error, not a fallback.
        // `Array(Any)` is such a binding too (array destructuring's element source).
        let declared = match ty {
            HirType::Any | HirType::Unknown => None,
            HirType::Array(element) if matches!(**element, HirType::Any) => None,
            ty => Some(self.shapes.value_type(ctx, ty)?),
        };
        let integer = self
            .shapes
            .is_explicit_binding(id)
            .then(|| self.shapes.integer_type(ctx, ty))
            .flatten();
        let expr_expected = integer
            .filter(|int_ty| int_ty.deref(ctx).width() == 64)
            .map(TypeHandle::from)
            .or(declared);
        let init = init
            .map(|e| self.expression_with(ctx, e, expr_expected))
            .transpose()?;
        // Inferred bindings preserve absence states and the selected reader mode.
        // HIR's getReader signature assumes the default overload.
        let carrier = match (declared, init) {
            (Some(ty), Some(value))
                if self.shapes.inferred_bindings.contains(&id)
                    && (optional_value(ctx, value.get_type(ctx))
                        .unwrap_or(value.get_type(ctx))
                        == optional_value(ctx, ty).unwrap_or(ty)
                        || (ty.deref(ctx).is::<ReaderType>()
                            && kind(ctx, value)? == Kind::ByobReader)) =>
            {
                value.get_type(ctx)
            }
            (Some(ty), _) => ty,
            (None, Some(value)) => value.get_type(ctx),
            (None, None) => bail!("an untyped binding needs an initializer"),
        };
        let binding = BindingType { carrier, integer };
        let slot = if self.shapes.global_ids.contains(&id) {
            self.shapes.globals.borrow_mut().insert(id, binding);
            self.global_slot(ctx, id, carrier)?
        } else {
            self.entry_slot(ctx, carrier)
        };
        if let Some(value) = init {
            let value = self.fit_binding(ctx, value, binding)?;
            self.store(ctx, slot, value);
        }
        self.locals.insert(id, LocalBinding { slot, integer });
        Ok(())
    }

    /// Lower an expression evaluated for its effects; void calls are allowed here.
    pub(super) fn effect(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<()> {
        if let Expr::Call {
            callee,
            args,
            byte_offset,
            ..
        } = expr
        {
            self.call(ctx, callee, args, *byte_offset)?;
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
    fn iterator_guard(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<bool> {
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
