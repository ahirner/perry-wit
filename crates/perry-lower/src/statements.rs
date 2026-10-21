//! Statements: bindings, `return`, `throw`, jumps and expression statements.
use super::*;
use crate::{builder::Jump, control_flow::LoopCondition};

impl<'s> FunctionBuilder<'s> {
    pub(super) fn statements(&mut self, ctx: &mut Context, statements: &'s [Stmt]) -> Result<()> {
        let mut rest = statements;
        while let Some((first, tail)) = rest.split_first() {
            rest = match self.iteration.element(rest) {
                Some(element) => {
                    let value = self.conditional(
                        ctx,
                        element.condition,
                        element.missing,
                        element.value,
                        None,
                    )?;
                    let slot = self.entry_slot(ctx, value.get_type(ctx));
                    self.store(ctx, slot, value);
                    self.locals.insert(element.id, LocalBinding::plain(slot));
                    let done = self.condition(ctx, element.condition)?;
                    let done = self.choose(
                        ctx,
                        done.get_type(ctx),
                        done,
                        |b, ctx| Ok(b.boolean(ctx, true)),
                        |b, ctx| {
                            let available = match element.available {
                                hir_compat::Availability::Test(test) => b.condition(ctx, test)?,
                                hir_compat::Availability::Known(available) => {
                                    b.boolean(ctx, available)
                                }
                            };
                            b.choose(
                                ctx,
                                available.get_type(ctx),
                                available,
                                |b, ctx| {
                                    if let Some(advance) = element.advance {
                                        b.effect(ctx, advance)?;
                                    }
                                    Ok(b.boolean(ctx, false))
                                },
                                |b, ctx| Ok(b.boolean(ctx, true)),
                            )
                        },
                    )?;
                    self.store(ctx, self.local(element.done)?, done);
                    element.rest
                }
                None => {
                    let prev_rest_keys = self.rest_excluded_keys;
                    if matches!(first, Stmt::Let { .. }) {
                        self.rest_excluded_keys = tail
                            .iter()
                            .take_while(|s| matches!(s, Stmt::Let { .. }))
                            .find_map(|s| match s {
                                Stmt::Let {
                                    init: Some(Expr::ObjectRest { exclude_keys, .. }),
                                    ..
                                } => Some(exclude_keys.as_slice()),
                                _ => None,
                            })
                            .unwrap_or(&[]);
                    }
                    let res = self
                        .statement(ctx, first, None)
                        .map_err(|error| match first {
                            Stmt::Let { id, .. } => self.shapes.annotate_local(error, *id),
                            _ => error,
                        });
                    self.rest_excluded_keys = prev_rest_keys;
                    res?;
                    tail
                }
            };
        }
        Ok(())
    }

    pub(super) fn statement(
        &mut self,
        ctx: &mut Context,
        statement: &'s Stmt,
        label: Option<&'s str>,
    ) -> Result<()> {
        match statement {
            Stmt::If {
                condition,
                else_branch,
                ..
            } if self.iteration.is_guard(condition) => {
                if let Some(else_branch) = else_branch {
                    self.statements(ctx, else_branch)?;
                }
            }
            Stmt::Let {
                id, name, ty, init, ..
            } => {
                self.iteration.register_binding(*id, name, init.as_ref());
                self.binding(ctx, *id, ty, init.as_ref())
                    .map_err(|error| self.shapes.annotate_local(error, *id))?;
            }
            Stmt::Expr(expr) => {
                self.effect(ctx, expr)?;
            }
            Stmt::Return(value) if !self.inline_returns.is_empty() => {
                self.inline_return(ctx, value.as_ref())?;
            }
            Stmt::Return(value) => {
                // An async function adopts a returned promise (and its result type) after
                // the finalizers ran.
                let (mut values, adopted) = match value {
                    Some(expr) => match self.result {
                        Some(result) => {
                            let value =
                                self.expression_with(ctx, expr, Some(result.expected_type(ctx)))?;
                            if self.is_async && value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                                (Vec::with_capacity(1), Some((value, result.carrier)))
                            } else {
                                (vec![self.fit_binding(ctx, value, result)?], None)
                            }
                        }
                        None => {
                            let is_never_expr = match expr {
                                Expr::LocalGet(id) => {
                                    self.locals.get(id).is_some_and(|b| b.is_never)
                                }
                                Expr::Call { callee, .. } => {
                                    if let Expr::FuncRef(fid) = callee.as_ref() {
                                        self.signatures.get(fid).is_some_and(|s| s.returns_never)
                                    } else {
                                        false
                                    }
                                }
                                _ => false,
                            };
                            ensure!(is_never_expr, "a void function returns a value");
                            self.effect(ctx, expr)?;
                            (self.implicit_results(ctx), None)
                        }
                    },
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
            Stmt::While { condition, body } => self.loop_(
                ctx,
                None,
                LoopCondition::Before(Some(condition)),
                None,
                body,
                label,
            )?,
            Stmt::DoWhile { body, condition } => self.loop_(
                ctx,
                None,
                LoopCondition::After(condition),
                None,
                body,
                label,
            )?,
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => self.loop_(
                ctx,
                init.as_deref(),
                LoopCondition::Before(condition.as_ref()),
                update.as_ref(),
                body,
                label,
            )?,
            Stmt::Switch {
                discriminant,
                cases,
            } => self.switch(ctx, discriminant, cases, label)?,
            Stmt::Break | Stmt::Continue | Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => {
                let (jump, label) = match statement {
                    Stmt::LabeledBreak(label) => (Jump::Break, Some(label.as_str())),
                    Stmt::LabeledContinue(label) => (Jump::Continue, Some(label.as_str())),
                    Stmt::Break => (Jump::Break, None),
                    _ => (Jump::Continue, None),
                };
                let (index, target) = self
                    .controls
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, control)| {
                        control.target(jump, label).map(|target| (index, target))
                    })
                    .with_context(|| match label {
                        Some(label) => format!(
                            "{} {label} has no enclosing target",
                            match jump {
                                Jump::Break => "break",
                                Jump::Continue => "continue",
                            }
                        ),
                        None => "break/continue has no enclosing target".into(),
                    })?;
                self.run_finallies(ctx, |f| f.controls > index)?;
                self.jump(ctx, target);
                self.after_terminator(ctx);
            }
            Stmt::Labeled { label, body } => {
                if matches!(
                    body.as_ref(),
                    Stmt::For { .. }
                        | Stmt::While { .. }
                        | Stmt::DoWhile { .. }
                        | Stmt::Switch { .. }
                ) {
                    self.statement(ctx, body, Some(label))?;
                } else {
                    let exit = self.new_block(ctx, "labeled_exit");
                    self.controls.push(Control::Labeled {
                        label: label.as_str(),
                        break_to: exit,
                    });
                    let res = self.statement(ctx, body, None);
                    self.controls.pop();
                    res?;
                    self.jump(ctx, exit);
                    self.switch_to(exit);
                }
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
            Stmt::PreallocateBoxes { .. } | Stmt::PreallocateTdzBoxes(_) => {}
            // Perry HIR exposes no variant names, only `Debug`: show the whole node.
            other => bail!("MIR backend does not support statement {other:?} yet"),
        }
        Ok(())
    }

    pub(super) fn binding(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        ty: &HirType,
        init: Option<&'s Expr>,
    ) -> Result<()> {
        if let Some(expr @ (Expr::Closure { .. } | Expr::FuncRef(_))) = init {
            self.closures.insert(id, expr);
            return Ok(());
        }
        if let Some(init) = init
            && let Some(source) = self.iteration.guard(id, init)
        {
            if let Some(source) = source {
                let source = self.expression(ctx, source)?;
                let source = self.defined(ctx, source)?;
                let tuple = TypedHandle::<RecordType>::from_handle(source.get_type(ctx), ctx)
                    .is_ok_and(|record| record.deref(ctx).kind() == RecordKind::Tuple);
                ensure!(
                    kind(ctx, source)? == Kind::Array || tuple,
                    "array destructuring is supported for arrays so far"
                );
            }
            return Ok(());
        }
        // Perry HIR leaves some bindings untyped (`Any`); those take the initializer's
        // type. An explicit type the subset cannot represent is an error, not a fallback.
        // `Array(Any)` is such a binding too (array destructuring's element source).
        let declared = match ty {
            HirType::Any | HirType::Unknown | HirType::Never | HirType::TypeVar(_) => None,
            HirType::Array(element)
                if matches!(
                    **element,
                    HirType::Any | HirType::Unknown | HirType::Never | HirType::TypeVar(_)
                ) =>
            {
                None
            }
            HirType::Generic { base, type_args }
                if matches!(
                    base.as_str(),
                    "Map"
                        | "ReadonlyMap"
                        | "MapKeys"
                        | "MapValues"
                        | "MapEntries"
                        | "Set"
                        | "ReadonlySet"
                        | "SetValues"
                        | "SetEntries"
                        | "Array"
                        | "ReadonlyArray"
                        | "Record"
                ) && (type_args.is_empty()
                    || type_args.iter().any(|t| {
                        matches!(t, HirType::Any | HirType::Unknown | HirType::TypeVar(_))
                    })) =>
            {
                None
            }
            ty if !self.shapes.is_explicit_binding(id) && infer::contains_type_var(ty) => None,
            ty => Some(self.shapes.value_type(ctx, ty)?),
        };
        let mut integer = self.shapes.integer_type(ctx, ty).filter(|int_ty| {
            self.shapes.is_explicit_binding(id) || int_ty.deref(ctx).width() == 64
        });
        let expr_expected = integer
            .filter(|int_ty| int_ty.deref(ctx).width() == 64)
            .map(TypeHandle::from)
            .or(declared);
        let init = init
            .map(|e| self.expression_with(ctx, e, expr_expected))
            .transpose()?;
        if integer.is_none()
            && let Some(value) = init
            && let Some(src) = self.integer64_source(ctx, value)
        {
            integer = TypedHandle::<IntegerType>::from_handle(src.get_type(ctx), ctx).ok();
        }
        let is_never = matches!(ty, HirType::Never);
        // Inferred bindings preserve absence states and the selected reader mode.
        // HIR's getReader signature assumes the default overload.
        let carrier = match (declared, init) {
            (Some(ty), Some(value))
                if self.shapes.metadata.inferred_bindings.contains(&id)
                    && (present_type(ctx, value.get_type(ctx)) == present_type(ctx, ty)
                        || (ty.deref(ctx).is::<ReaderType>()
                            && kind(ctx, value)? == Kind::ByobReader)) =>
            {
                value.get_type(ctx)
            }
            (Some(ty), _) => ty,
            (None, Some(value)) => value.get_type(ctx),
            (None, None) if is_never => self.shapes.value_type(ctx, ty)?,
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
        } else if is_never {
            let undef = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, carrier));
            self.store(ctx, slot, undef);
        }
        self.locals.insert(
            id,
            LocalBinding {
                slot,
                integer,
                is_never,
            },
        );
        Ok(())
    }

    /// Lower an expression statement evaluated for its side effects, permitting `void` calls.
    pub(super) fn effect(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<()> {
        if self.iteration.is_release(expr) {
            return Ok(());
        }
        if let Expr::Call {
            callee,
            args,
            byte_offset,
            ..
        } = expr
        {
            self.call(ctx, callee, args, *byte_offset)
                .map_err(|error| self.shapes.annotate_call(error, *byte_offset))?;
        } else if let Expr::StaticMethodCall {
            class_name,
            method_name,
            args,
        } = expr
        {
            self.class_static_method_call(ctx, class_name, method_name, args)?;
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
}
