//! Lower a solved operation at its lexical expression.
use super::*;
use crate::engine::solve::{SiteEntry, SiteResolution};
use crate::engine::types::Ty;
use crate::source::ConstructorSites;

impl<'s> FunctionBuilder<'s, '_> {
    pub(in crate::lower) fn site(&self, expr: &Expr) -> Result<SiteEntry> {
        self.shapes
            .specialization
            .instances
            .site(self.frame.owner, expr)
    }

    pub(in crate::lower) fn erased_operand(&self, expr: &Expr) -> Result<bool> {
        Ok(self.site(expr)?.erased)
    }

    pub(in crate::lower) fn instance_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        required: bool,
    ) -> Result<Option<Value>> {
        let site = self.site(expr)?;
        let result = if let Some(conversion) = site.conversion() {
            if !conversion.reads_operand() {
                self.discard_function_operand(ctx, expr)?;
            }
            conversion
                .emit(self, ctx, expected, |b, ctx, input| {
                    b.instance_value(ctx, expr, input, site.resolution, true)?
                        .ok_or_else(|| internal!("missing function operand"))
                })
                .map(Some)
        } else {
            self.instance_value(ctx, expr, expected, site.resolution, required)
        };
        result.map_err(|e| self.shapes.locations.annotate_expression(e, expr))
    }
    pub(in crate::lower) fn instance_value(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        site: Option<SiteResolution>,
        required: bool,
    ) -> Result<Option<Value>> {
        let plans = &self.shapes.specialization.instances;
        if let Some(id) = plans.reference_at(self.frame.owner, expr, site)? {
            let value = self
                .shapes
                .specialization
                .closure_value(self.frame.owner, id)
                .ok_or_else(|| unsupported!("this expression form is not supported yet"))?;
            return self.closure_value(ctx, value, expected).map(Some);
        }
        use SiteResolution::*;
        let (get, set) = match site {
            Some(ReadMember(id)) => (Some(id), None),
            Some(WriteMember(id)) => (None, Some(id)),
            Some(UpdateMember { get, set }) => (get, set),
            None | Some(Cell(_)) => (None, None),
            Some(TypedArray(_)) if expected.is_some() => (None, None),
            Some(site) => return self.solved_value(ctx, expr, expected, site, required),
        };
        if let Some((object, key)) = hir_compat::member(expr) {
            if let (Expr::ClassRef(class), Some(property)) = (object, key.name()) {
                return self
                    .load_local(ctx, self.shapes.static_field(object, class, property)?)
                    .map(Some);
            }
            let receiver = self
                .expression(ctx, object)
                .map_err(|e| self.shapes.annotate_member(e, key.offset()))?;
            return self.read_member(ctx, object, receiver, key, get);
        }
        if let Some((object, key, value)) = hir_compat::member_assignment(expr) {
            return match key {
                hir_compat::MemberKey::Named(name, _) => {
                    self.set_field(ctx, object, name, value, set)
                }
                hir_compat::MemberKey::Computed(index, _) => {
                    self.set_index(ctx, object, index, value).map(Some)
                }
            };
        }
        let value = match expr {
            Expr::PropertyUpdate {
                object,
                property,
                op,
                prefix,
                ..
            } => self.property_update(ctx, object, property, op, *prefix, (get, set))?,
            Expr::StringCoerce(object) | Expr::TemplateStringCoerce(object)
                if let Some(id) = get =>
            {
                let object = self.expression(ctx, object)?;
                Member::Accessor(self.defined(ctx, object)?, id).read(self, ctx)?
            }
            _ => return self.source_operand(ctx, expr, expected, required),
        };
        Ok(Some(value))
    }

    fn solved_value(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        site: SiteResolution,
        required: bool,
    ) -> Result<Option<Value>> {
        use SiteResolution::*;
        let plans = &self.shapes.specialization.instances;
        let result = match (site, expr) {
            (Regex(literal), _) => {
                let literal = literal.ok_or_else(|| {
                    unsupported!("RegExp construction is unsupported; use a bounded regex literal")
                })?;
                for arg in plans
                    .regexp(expr)
                    .into_iter()
                    .flat_map(|args| args.operands())
                {
                    self.effect(ctx, arg)?;
                }
                literal.parsed(ctx)?;
                self.emit_value(perry_mir::RegexConstOp::from_literal(ctx, literal), ctx)
            }
            (Environment(value), _) => self.closure_value(ctx, value, expected)?,
            (ObjectSpread(ty), Expr::ObjectSpread { parts }) => {
                self.object_spread(ctx, parts, expected, Some(ty))?
            }
            (BranchResult(ty) | TypedArray(ty), _) => {
                let ty = match expected {
                    Some(ty) if crate::lower::value_unions::is_value_union(ctx, ty) => ty,
                    _ => self.shapes.resolved_binding(ty)?.carrier,
                };
                return self.source_operand(ctx, expr, Some(ty), required);
            }
            (Call(target), call) => {
                let call = crate::source::SourceCall::of(call)
                    .ok_or_else(|| internal!("solved call requires a call expression"))?;
                return self
                    .invoke_site(ctx, target, call)
                    .map_err(|e| self.shapes.annotate_call(e, call.offset));
            }
            (
                PromiseNew {
                    value_loc,
                    executor: Some(executor),
                    ..
                },
                Expr::New { args, .. },
            ) => self.new_promise(ctx, value_loc, executor, args)?,
            (
                ReadFile { text },
                Expr::Call {
                    args, byte_offset, ..
                },
            ) => self
                .read_file(ctx, args, text)
                .map_err(|e| self.shapes.annotate_call(e, *byte_offset))?,
            (CollectionNew(ty), _) => {
                let ty = self.shapes.resolved_binding(ty)?.carrier;
                return self.source_operand(ctx, expr, Some(ty), required);
            }
            (
                PromiseReaction { helper, .. },
                Expr::Call {
                    callee,
                    args,
                    byte_offset,
                    ..
                },
            ) if let Some((receiver, _)) = hir_compat::member(callee) => {
                let arguments = std::iter::once((receiver, false)).chain(Arguments::Plain(args));
                return self
                    .invoke_arguments(
                        ctx,
                        self.signatures.solved.helper(helper)?,
                        std::iter::empty(),
                        arguments,
                        None,
                    )
                    .map_err(|e| self.shapes.annotate_call(e, *byte_offset));
            }
            (
                TimerStart { helper },
                Expr::Call {
                    args, byte_offset, ..
                },
            ) => self
                .start_timer(ctx, self.signatures.solved.helper(helper)?, args)
                .map_err(|e| self.shapes.annotate_call(e, *byte_offset))?,
            (
                TimerClear { arg_ty },
                Expr::Call {
                    args, byte_offset, ..
                },
            ) => {
                self.clear_timer_args(
                    ctx,
                    arg_ty.is_some_and(|t| !matches!(t, Ty::VOID | Ty::NULL)),
                    args,
                )
                .map_err(|e| self.shapes.annotate_call(e, *byte_offset))?;
                if !required {
                    return Ok(None);
                }
                self.absent_literal(ctx, false, expected)?
            }
            (
                Builtin(method),
                Expr::Call {
                    callee,
                    args,
                    byte_offset,
                    ..
                },
            ) if let Some((receiver, hir_compat::MemberKey::Named(name, _))) =
                hir_compat::member(callee) =>
            {
                return self
                    .builtin_call(
                        ctx,
                        receiver,
                        method,
                        hir_compat::method(name).name(),
                        args,
                        Some(*byte_offset),
                    )
                    .map_err(|e| self.shapes.annotate_call(e, *byte_offset));
            }
            (
                Builtin(selected),
                Expr::NativeMethodCall {
                    object: Some(receiver),
                    method,
                    args,
                    ..
                },
            ) => {
                return self.builtin_call(ctx, receiver, selected, method, args, None);
            }
            (
                Static(method),
                Expr::Call {
                    args, byte_offset, ..
                },
            ) => self
                .static_call(ctx, method, args)
                .map_err(|e| self.shapes.annotate_call(e, *byte_offset))?,
            (ResourceCall(id), _) => {
                let (args, offset) = hir_compat::call_arguments(expr)
                    .ok_or_else(|| internal!("expected a member invocation"))?;
                let receiver = if let Expr::Call { callee, .. } = expr
                    && let Some((object, property)) = hir_compat::member(callee)
                    && (property.name().is_none() || hir_compat::class_receiver(object).is_none())
                {
                    let value = self.expression(ctx, object)?;
                    Some(self.defined(ctx, value)?)
                } else {
                    None
                };
                return self
                    .resource_call(ctx, id, receiver, args)
                    .map_err(|error| self.shapes.annotate_call(error, offset));
            }
            (
                ClassNew(id),
                Expr::New {
                    args, byte_offset, ..
                },
            ) => {
                let class = &self.shapes.specialization.bodies.classes[id];
                self.class_new(ctx, class, args)
                    .map_err(|e| self.shapes.annotate_call(e, *byte_offset))?
            }
            (
                ZeroSizedFieldSet,
                Expr::PropertySet { object, value, .. }
                | Expr::PutValueSet {
                    target: object,
                    value,
                    ..
                },
            ) => {
                self.discard_function_operand(ctx, object)?;
                self.discard_function_operand(ctx, value)?;
                return if required {
                    self.absent_literal(ctx, false, expected).map(Some)
                } else {
                    Ok(None)
                };
            }
            (FoldBool(value), Expr::Compare { left, right, .. }) => {
                self.discard_function_operand(ctx, left)?;
                self.discard_function_operand(ctx, right)?;
                self.boolean(ctx, value)
            }
            (FuncTypeOf(ty), Expr::TypeOf(operand)) => match ty {
                Some(ty) => self.callable_typeof(ctx, operand, ty)?,
                None => {
                    self.discard_function_operand(ctx, operand)?;
                    self.string(ctx, "function")
                }
            },
            (
                Matches {
                    identity,
                    source,
                    left_erased,
                    equal,
                },
                Expr::Compare { left, right, .. },
            ) => {
                if left_erased {
                    self.discard_function_operand(ctx, left)?;
                    self.matches_function(ctx, right, source, identity, equal)?
                } else {
                    let value = self.matches_function(ctx, left, source, identity, equal)?;
                    self.discard_function_operand(ctx, right)?;
                    value
                }
            }
            (
                FuncCompare {
                    left_ty,
                    right_ty,
                    equal,
                    environments,
                },
                Expr::Compare { left, right, .. },
            ) => {
                let same = if environments {
                    let a = self.expression(ctx, left)?;
                    let a = self.record_field(ctx, a, "val")?;
                    let b = self.expression(ctx, right)?;
                    let b = self.record_field(ctx, b, "val")?;
                    self.compare(ctx, CompareKind::Eq, a, b)
                } else {
                    self.compare_callables(ctx, left, right, left_ty, right_ty)?
                };
                if equal { same } else { self.not(ctx, same) }
            }
            _ => bail_internal!("solved operation does not match its source expression"),
        };
        Ok(Some(result))
    }
}
