//! Statements: bindings, `return`, `throw`, jumps and expression statements.
use super::*;
use crate::engine::types::LocalFlags;
use crate::engine::{CellInit, Storage};
use crate::lower::control_flow::{LoopCondition, LoopUpdate};

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn statements(&mut self, ctx: &mut Context, statements: &'s [Stmt]) -> Result<()> {
        for statement in statements {
            if let Stmt::Let { id, .. } = statement
                && let Some(
                    storage @ Storage::Boxed {
                        init: CellInit::Hoisted,
                        ..
                    },
                ) = self.local_storage(*id)
            {
                self.binding(ctx, *id, None, Some(storage))
                    .map_err(|error| self.shapes.annotate_local(error, *id))?;
            }
        }
        let mut rest = statements;
        let mut loop_cells = Vec::new();
        while let Some((first, tail)) = rest.split_first() {
            rest = match self.shapes.specialization.instances.locals.element(rest) {
                Some((element, rest)) => {
                    let element_ty = self
                        .declared_local(ctx, self.local_storage(element.id), element.id)?
                        .0
                        .carrier;
                    let element_ty = crate::lower::coerce::admitting(
                        ctx,
                        element_ty,
                        perry_mir::Absence::Undefined,
                    );
                    let done = self.condition(ctx, element.condition)?;
                    let available = self.choose(
                        ctx,
                        done.get_type(ctx),
                        done,
                        |b, ctx| Ok(b.boolean(ctx, false)),
                        |b, ctx| match element.available {
                            hir_compat::Availability::Test(test) => b.condition(ctx, test),
                            hir_compat::Availability::Known(value) => Ok(b.boolean(ctx, value)),
                        },
                    )?;
                    let value = self.choose(
                        ctx,
                        element_ty,
                        available,
                        |b, ctx| {
                            let value = b.expression_with(ctx, element.value, Some(element_ty))?;
                            if let Some(advance) = element.advance {
                                b.effect(ctx, advance)?;
                            }
                            Ok(value)
                        },
                        |b, ctx| b.expression_with(ctx, element.missing, Some(element_ty)),
                    )?;
                    let binding = self.initialized_local(ctx, value);
                    self.variant_guards.assign(element.id);
                    self.locals.insert(element.id, binding);
                    let done = self.not(ctx, available);
                    self.store(ctx, self.local(element.done)?, done);
                    rest
                }
                None => {
                    self.statement(ctx, first, None, &loop_cells)
                        .map_err(|error| match first {
                            Stmt::Let { id, .. } => self.shapes.annotate_local(error, *id),
                            _ => error,
                        })?;
                    tail
                }
            };
            match first {
                Stmt::Let { id, .. } if self.per_iteration(*id) => loop_cells.push(*id),
                _ => loop_cells.clear(),
            }
        }
        Ok(())
    }

    pub(super) fn statement(
        &mut self,
        ctx: &mut Context,
        statement: &'s Stmt,
        label: Option<&'s str>,
        loop_cells: &[LocalId],
    ) -> Result<()> {
        match statement {
            Stmt::If {
                condition: Expr::ThisProvenClass(class_name),
                then_branch,
                else_branch,
            } => {
                if self.is_proven_this_class(ctx, class_name) {
                    self.statements(ctx, then_branch)?;
                } else if let Some(else_branch) = else_branch {
                    self.statements(ctx, else_branch)?;
                }
            }
            Stmt::If {
                condition,
                else_branch,
                ..
            } if self
                .shapes
                .specialization
                .instances
                .locals
                .is_guard(condition) =>
            {
                if let Some(else_branch) = else_branch {
                    self.statements(ctx, else_branch)?;
                }
            }
            Stmt::Let { id, init, .. } => {
                let storage = self.local_storage(*id);
                if matches!(storage, Some(Storage::ErasedFunction(_))) {
                    if let Some(expr) = init {
                        self.discard_function_operand(ctx, expr)?;
                    }
                    return Ok(());
                }
                let init = init.as_ref();
                if matches!(
                    storage,
                    Some(Storage::Boxed {
                        init: CellInit::Hoisted,
                        ..
                    })
                ) {
                    if let Some(value) = init {
                        self.local_set(ctx, *id, value)?;
                    }
                } else {
                    self.binding(ctx, *id, init, storage)
                        .map_err(|error| self.shapes.annotate_local(error, *id))?;
                }
            }
            Stmt::Expr(expr) => {
                self.effect(ctx, expr)?;
            }
            Stmt::Return(value) => {
                let erased = matches!(self.frame.owner, BodyOwner::Function(id) if self.signatures.named.get(&id).and_then(Signature::source)
                    .is_some_and(|s| matches!(s.result, crate::engine::InstanceResult::ErasedFunction)));
                let value = if erased {
                    if let Some(expr) = value {
                        self.discard_function_operand(ctx, expr)?;
                    }
                    None
                } else {
                    value.as_ref()
                };
                if let Some(frame) = self.frame.return_to {
                    return self.inline_return(ctx, frame, value);
                }
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
                            ensure_diag!(
                                self.optional_operand(ctx, expr, None)?.is_none(),
                                codes::TYPE_MISMATCH,
                                "a void function returns a value"
                            );
                            (self.implicit_results(ctx), None)
                        }
                    },
                    None => (self.implicit_results(ctx), None),
                };
                self.run_finallies(ctx, 0)?;
                if self.terminated(ctx) {
                    self.after_terminator(ctx);
                    return Ok(());
                }
                if let Some((promise, expected)) = adopted {
                    // Returning a promise runs finalizers before adoption; its later rejection
                    // does not enter a catch surrounding the return expression.
                    let value = self.with_handler(ctx, None, None, |b, ctx| {
                        let await_ = AwaitOp::new(ctx, promise).to_diag(ctx)?;
                        let value = b.emit_value(await_, ctx);
                        b.fit(ctx, value, expected)
                    })?;
                    values.push(value);
                }
                ensure_diag!(
                    values
                        .iter()
                        .map(|v| v.get_type(ctx))
                        .eq(self.result.iter().map(|r| r.carrier)),
                    codes::TYPE_MISMATCH,
                    "return value type differs from the declared result"
                );
                self.emit(ReturnOp::new(ctx, values), ctx);
                self.after_terminator(ctx);
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
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
                // Stores in one arm do not reach the other arm, nor the join unless the arm
                // falls through to it.
                let arms = self.variant_guards.branch();
                self.variant_guards.condition(condition, true);
                self.statements(ctx, then_branch)?;
                let then_reachable = self.reachable(ctx, self.block);
                self.jump(ctx, join);
                let then_stores = self.variant_guards.other_arm(arms);
                let mut else_reachable = true;
                if let Some(else_branch) = else_branch {
                    self.switch_to(otherwise);
                    self.variant_guards.condition(condition, false);
                    self.statements(ctx, else_branch)?;
                    else_reachable = self.reachable(ctx, self.block);
                    self.jump(ctx, join);
                }
                if let Some(truth) =
                    self.variant_guards
                        .join(arms, then_stores, then_reachable, else_reachable)
                {
                    self.variant_guards.condition(condition, truth);
                }
                self.switch_to(join);
            }
            Stmt::While { condition, body } => self.loop_(
                ctx,
                None,
                LoopCondition::Before(Some(condition)),
                LoopUpdate::default(),
                body,
                label,
            )?,
            Stmt::DoWhile { body, condition } => self.loop_(
                ctx,
                None,
                LoopCondition::After(condition),
                LoopUpdate::default(),
                body,
                label,
            )?,
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some((id, source)) = self
                    .shapes
                    .specialization
                    .instances
                    .locals
                    .for_of(init.as_deref())
                {
                    return self.for_of(ctx, id, source, body, label);
                }
                let mut cells = loop_cells.to_vec();
                if let Some(Stmt::Let { id, .. }) = init.as_deref()
                    && self.per_iteration(*id)
                {
                    cells.push(*id);
                }
                self.loop_(
                    ctx,
                    init.as_deref(),
                    LoopCondition::Before(condition.as_ref()),
                    LoopUpdate {
                        expression: update.as_ref(),
                        cells,
                    },
                    body,
                    label,
                )?;
            }
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
                let (depth, target) = self.frame.jump_target(jump, label)?;
                self.run_finallies(ctx, depth)?;
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
                    self.statement(ctx, body, Some(label), loop_cells)?;
                } else {
                    let exit = self.new_block(ctx, "labeled_exit");
                    self.frame.controls.push(Control::Labeled {
                        label: label.as_str(),
                        break_to: exit,
                    });
                    // A `break` reaches the exit before the narrowings that follow it.
                    let guards = self.variant_guards.depth();
                    let res = self.statements(ctx, std::slice::from_ref(body));
                    self.variant_guards.truncate(guards);
                    self.frame.controls.pop();
                    res?;
                    self.jump(ctx, exit);
                    self.switch_to(exit);
                }
            }
            Stmt::Throw(value) => {
                let value = self.expression(ctx, value)?;
                self.throw_value(ctx, value)?;
            }
            Stmt::Try {
                body,
                catch,
                finally,
                ..
            } => self.try_(ctx, body, catch.as_ref(), finally.as_deref())?,
            Stmt::PreallocateBoxes { .. } | Stmt::PreallocateTdzBoxes(_) => {}
            _ => bail_unsupported!("this statement form is not supported yet"),
        }
        Ok(())
    }

    pub(super) fn throw_value(&mut self, ctx: &mut Context, value: Value) -> Result<()> {
        ensure_unsupported!(
            matches!(
                kind(ctx, value).or_else(|_| optional_value(ctx, value.get_type(ctx))
                    .map(|_| Kind::Number)
                    .ok_or_else(|| Diagnostic::unsupported("unsupported thrown value")))?,
                Kind::Exception
                    | Kind::Json
                    | Kind::Number
                    | Kind::Boolean
                    | Kind::String
                    | Kind::BigInt
            ),
            "only Error objects, numbers, bigints, booleans and strings can be thrown"
        );
        let status = self.emit_value(RaiseOp::new(ctx, value), ctx);
        self.propagate(ctx, status);
        self.after_terminator(ctx);
        Ok(())
    }

    fn per_iteration(&self, id: LocalId) -> bool {
        matches!(
            self.local_storage(id),
            Some(crate::engine::Storage::Boxed { .. })
        ) && self
            .shapes
            .specialization
            .instances
            .locals
            .row(id)
            .is_some_and(|binding| binding.flags.contains(LocalFlags::FOR_HEAD))
    }

    pub(super) fn local_storage(&self, id: LocalId) -> Option<crate::engine::Storage> {
        self.shapes
            .specialization
            .instances
            .storage(self.frame.owner, id)
    }

    fn declared_local(
        &mut self,
        ctx: &Context,
        storage: Option<crate::engine::Storage>,
        id: LocalId,
    ) -> Result<(BindingType, bool)> {
        use crate::engine::{Storage, types::Ty};
        if let Some(storage) = storage {
            return Ok((
                self.shapes.storage_binding(ctx, storage)?,
                matches!(storage, Storage::Value(Ty::NEVER)),
            ));
        }
        let ty = self.shapes.specialization.instances.local(id)?.annotation;
        Ok((self.shapes.resolved_binding(ty)?, ty == Ty::NEVER))
    }

    pub(super) fn binding(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        init: Option<&'s Expr>,
        storage: Option<crate::engine::Storage>,
    ) -> Result<()> {
        if let Some(crate::engine::Storage::Boxed {
            ty,
            init: cell_init,
        }) = storage
        {
            let deferred = cell_init.admits_undefined();
            let (record, payload) = self.shapes.box_layout(ctx, ty, deferred)?;
            let value = match init {
                Some(expr) => self.expression_with(ctx, expr, Some(payload))?,
                None if deferred || perry_mir::admits_undefined(ctx, payload) => {
                    self.absent_literal(ctx, false, Some(payload))?
                }
                None if perry_mir::absence(ctx, payload).is_some_and(|a| a.admits_null()) => {
                    self.absent_literal(ctx, true, Some(payload))?
                }
                None => match ty {
                    crate::engine::types::Ty::NUMBER | crate::engine::types::Ty::INT32 => {
                        self.number(ctx, 0.0)
                    }
                    crate::engine::types::Ty::BOOLEAN => self.boolean(ctx, false),
                    crate::engine::types::Ty::STRING => self.string(ctx, ""),
                    _ => {
                        return Err(Diagnostic::unsupported(
                            "captured local declared without an initializer has no placeholder value",
                        ));
                    }
                },
            };
            let value = self.fit(ctx, value, payload)?;
            let value = self.emit_value(AllocOp::new(ctx, record, vec![value]), ctx);
            let ty = record.into();
            let layout =
                LocalBinding::slot(ty, BindingType::plain(ty), false).captured(payload, deferred);
            let slot = self.declare_slot(ctx, id, layout)?;
            self.store(ctx, slot, value);
            return Ok(());
        }
        if let Some(expr) = init {
            let source = self.shapes.specialization;
            let site = self.site(expr)?;
            let erased = match expr {
                Expr::Closure { .. } => site.environment().is_none(),
                _ => source
                    .instances
                    .reference_at(self.frame.owner, expr, site.resolution)?
                    .is_some_and(|id| source.closure_value(self.frame.owner, id).is_none()),
            };
            if erased && site.conversion().is_none() {
                return Ok(());
            }
        }
        if let Some(source) = self.shapes.specialization.instances.locals.guard(id) {
            if let Some(source) = source {
                let source = self.expression(ctx, source)?;
                let source = self.defined(ctx, source)?;
                let tuple = perry_mir::typed::<RecordType>(ctx, source.get_type(ctx))
                    .is_some_and(|record| record.deref(ctx).kind() == RecordKind::Tuple);
                ensure_unsupported!(
                    kind(ctx, source)? == Kind::Array || tuple,
                    "array destructuring is supported for arrays so far"
                );
            }
            return Ok(());
        }
        let flags = self.shapes.specialization.instances.local(id)?.flags;
        let explicit = flags.is_explicit();
        let (mut binding, is_never) = match self.declared_local(ctx, storage, id) {
            Err(error)
                if error.code() == codes::MISSING_TYPE_ANNOTATION
                    && let Some(name) = self.shapes.local_name(id) =>
            {
                return Err(diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "cannot infer the type of binding '{name}'"
                )
                .with_help("Add a concrete type annotation to this binding."));
            }
            Err(error)
                if !explicit
                    && flags.contains(LocalFlags::DEFERRED)
                    && error.code() == codes::UNSUPPORTED_TYPE =>
            {
                let ty = match storage {
                    Some(crate::engine::Storage::Value(ty)) => ty,
                    _ => self.shapes.specialization.instances.local(id)?.annotation,
                };
                let facts = self.shapes.specialization;
                if self.shapes.types.source.is_json_encodable_union(
                    ty,
                    &facts.locations,
                    &facts.dict_reads,
                ) {
                    (
                        BindingType::plain(perry_mir::JsonType::get(ctx).into()),
                        false,
                    )
                } else {
                    let name = self.shapes.local_name(id).unwrap_or("binding");
                    return Err(diag!(
                        codes::UNSUPPORTED_TYPE,
                        "local '{name}' receives incompatible write types ({})",
                        error.message()
                    ));
                }
            }
            result => result?,
        };
        let declared = binding.carrier;
        binding.integer = binding
            .integer
            .filter(|ty| explicit || ty.deref(ctx).width() == 64);
        let deferred = self.shapes.deferred_storage(ctx, flags, declared);
        let init = if is_never {
            if let Some(expr) = init {
                self.effect(ctx, expr)?;
            }
            None
        } else if deferred.is_some() {
            None
        } else {
            init.map(|e| self.expression_with(ctx, e, Some(binding.expected_type(ctx))))
                .transpose()?
        };
        if binding.integer.is_none()
            && let Some(src) = init.and_then(|v| self.integer64_source(ctx, v))
        {
            binding.integer = perry_mir::typed::<IntegerType>(ctx, src.get_type(ctx));
        }
        if let Some(value) = init
            && flags.contains(LocalFlags::INFERRED)
        {
            binding.carrier = crate::lower::coerce::result_carrier(ctx, declared, &[value]);
        }
        let storage = deferred.unwrap_or(binding.carrier);
        let slot = self.declare_slot(ctx, id, LocalBinding::slot(storage, binding, is_never))?;
        if let Some(value) = init {
            let value = self.fit_binding(ctx, value, binding)?;
            self.store(ctx, slot, value);
        } else if is_never || deferred.is_some() {
            let undef = self.emit_value(UndefinedOp::with_type(ctx, storage), ctx);
            self.store(ctx, slot, undef);
        }
        Ok(())
    }

    pub(super) fn optional_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Option<Value>> {
        let result = self.instance_operand(ctx, expr, expected, false)?;
        Ok(result.filter(|value| !self.never_values.contains(value)))
    }

    pub(super) fn getter(&self, expr: &Expr) -> Result<Option<crate::engine::types::InstanceId>> {
        Ok(match self.site(expr)?.resolution {
            Some(crate::engine::solve::SiteResolution::ReadMember(id)) => Some(id),
            _ => None,
        })
    }

    pub(super) fn discard_function_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
    ) -> Result<()> {
        match expr {
            Expr::FuncRef(_)
            | Expr::Closure { .. }
            | Expr::LocalGet(_)
            | Expr::This
            | Expr::ClassRef(_) => Ok(()),
            Expr::PropertyGet {
                object,
                byte_offset,
                ..
            } if self.getter(expr)?.is_none() => {
                if hir_compat::class_receiver(object).is_none() {
                    let receiver = self.expression(ctx, object)?;
                    self.receiver(ctx, receiver, *byte_offset)?;
                }
                Ok(())
            }
            Expr::IndexGet { object, index, .. }
                if hir_compat::class_receiver(object).is_some() =>
            {
                self.effect(ctx, index)
            }
            Expr::ExternFuncRef { .. } => Ok(()),
            _ => {
                self.instance_value(ctx, expr, None, self.site(expr)?.resolution, false)?;
                Ok(())
            }
        }
    }

    pub(super) fn effect(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<()> {
        self.optional_operand(ctx, expr, None).map(|_| ())
    }
}
