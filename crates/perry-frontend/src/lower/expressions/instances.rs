//! Lower a solved operation at its lexical expression.
use super::*;
use crate::engine::solve::{OperandUse, SiteEntry, SiteResolution};
use crate::engine::types::Ty;

impl<'s> FunctionBuilder<'s, '_> {
    pub(in crate::lower) fn site(&self, expr: &Expr) -> SiteEntry {
        self.shapes
            .specialization
            .instances
            .site(self.frame.owner, expr)
    }

    pub(in crate::lower) fn erased_operand(&self, expr: &Expr) -> bool {
        self.site(expr).operand == OperandUse::Erased
    }

    pub(in crate::lower) fn instance_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        required: bool,
    ) -> Result<Option<Value>> {
        let site = self.site(expr);
        let Some(conversion) = site.conversion() else {
            return self.instance_value(ctx, expr, expected, site.resolution, required);
        };
        if !conversion.reads_operand() && !crate::source::is_pure_func_expr(expr) {
            self.instance_value(ctx, expr, None, site.resolution, false)?;
        }
        conversion
            .emit(self, ctx, expected, |b, ctx, input| {
                b.instance_value(ctx, expr, input, site.resolution, true)?
                    .ok_or_else(|| internal!("missing function operand"))
            })
            .map(Some)
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
        if let Some(id) = plans.reference_at(self.frame.owner, expr, site) {
            let value = self
                .shapes
                .specialization
                .closure_value(self.frame.owner, id)
                .ok_or_else(|| unsupported!("this expression form is not supported yet"))?;
            return self.closure_value(ctx, value, expected).map(Some);
        }
        use SiteResolution::*;
        let (get, set) = match site {
            Some(ReadMember(id)) => (Some(plans.function(id)), None),
            Some(WriteMember(id)) => (None, Some(plans.function(id))),
            Some(UpdateMember { get, set }) => (
                get.map(|id| plans.function(id)),
                set.map(|id| plans.function(id)),
            ),
            None | Some(Cell(_)) => (None, None),
            Some(TypedArray(_)) if expected.is_some() => (None, None),
            Some(site) => return self.solved_value(ctx, expr, expected, site, required),
        };
        let value = match expr {
            Expr::PropertyUpdate {
                object,
                property,
                op,
                prefix,
                ..
            } => self.property_update(ctx, object, property, op, *prefix, (get, set))?,
            Expr::PropertyGet {
                object,
                property,
                byte_offset,
                ..
            } => self
                .property_get(ctx, object, property, *byte_offset, get)
                .map_err(|error| self.shapes.annotate_member(error, *byte_offset))?,
            Expr::PropertySet {
                object,
                property,
                value,
                ..
            } => return self.set_field(ctx, object, property, value, set),
            Expr::PutValueSet {
                target,
                key,
                value,
                element_offset,
                ..
            } => {
                return match key.as_ref() {
                    Expr::String(field) => self.set_field(ctx, target, field, value, set),
                    index => self.set_index(ctx, target, index, value).map(Some),
                }
                .map_err(|error| self.shapes.annotate_member(error, *element_offset));
            }
            Expr::StringCoerce(object) | Expr::TemplateStringCoerce(object)
                if let Some(id) = get =>
            {
                let object = self.expression(ctx, object)?;
                Member::Accessor(self.defined(ctx, object)?, id).read(self, ctx)?
            }
            _ if required => self.source_expression(ctx, expr, expected)?,
            _ => return self.source_operand(ctx, expr, expected),
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
            (Environment(value), _) => self.closure_value(ctx, value, expected)?,
            (ObjectSpread(ty), Expr::ObjectSpread { parts }) => {
                self.object_spread(ctx, parts, expected, Some(ty))?
            }
            (BranchResult(ty) | TypedArray(ty), _) => {
                let ty = match expected {
                    Some(ty) if crate::lower::value_unions::is_value_union(ctx, ty) => ty,
                    _ => self.shapes.resolved_binding(ty)?.carrier,
                };
                self.source_expression(ctx, expr, Some(ty))?
            }
            (
                Call {
                    target,
                    argument_types,
                },
                call,
            ) => {
                let (callee, args, offset) = match call {
                    Expr::Call {
                        callee,
                        args,
                        byte_offset,
                        ..
                    } => (callee, Arguments::Plain(args), *byte_offset),
                    Expr::CallSpread {
                        callee,
                        args,
                        byte_offset,
                        ..
                    } => (callee, Arguments::Spread(args), *byte_offset),
                    Expr::StaticMethodCall { args, .. } => {
                        return target.invoke(
                            self,
                            ctx,
                            None,
                            None,
                            crate::lower::calls::CallArguments::Expressions(
                                Arguments::Plain(args),
                                argument_types,
                            ),
                        );
                    }
                    _ => bail_internal!("solved call requires a call expression"),
                };
                return self
                    .invoke_site(ctx, target, argument_types, callee, args)
                    .map_err(|e| self.shapes.annotate_call(e, offset));
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
                self.source_expression(ctx, expr, Some(ty))?
            }
            (
                PromiseReaction {
                    helper,
                    pass_first,
                    pass_second,
                    ..
                },
                Expr::Call {
                    callee,
                    args,
                    byte_offset,
                    ..
                },
            ) => {
                return self
                    .promise_reaction(
                        ctx,
                        plans.helper(helper)?,
                        [pass_first, pass_second],
                        callee,
                        args,
                    )
                    .map_err(|e| self.shapes.annotate_call(e, *byte_offset));
            }
            (
                TimerStart { helper },
                Expr::Call {
                    args, byte_offset, ..
                },
            ) => self
                .start_timer(ctx, plans.helper(helper)?, args)
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
            ) if let Some((receiver, Some(name))) = hir_compat::member(callee) => {
                return self
                    .builtin_call(ctx, receiver, method, hir_compat::method(name).name(), args)
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
                return self.builtin_call(ctx, receiver, selected, method, args);
            }
            (ResourceCall(id), _) => {
                let (args, offset) = hir_compat::call_arguments(expr)
                    .ok_or_else(|| internal!("expected a member invocation"))?;
                let receiver = if let Expr::Call { callee, .. } = expr
                    && let Some((object, property)) = hir_compat::member(callee)
                    && (property.is_none() || hir_compat::class_receiver(object).is_none())
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
                ClassNew(id, arguments),
                Expr::New {
                    args, byte_offset, ..
                },
            ) => {
                let class = &self.shapes.specialization.bodies.classes[id];
                self.class_new(ctx, class, args, Some(arguments))
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
