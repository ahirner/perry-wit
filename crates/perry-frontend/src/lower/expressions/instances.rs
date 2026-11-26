//! Lower a solved operation at its lexical expression.
use super::*;
use crate::engine::solve::{FuncCoercion, SiteResolution, ValueCoercion};
use crate::engine::types::Ty;

impl<'s> FunctionBuilder<'s, '_> {
    pub(in crate::lower) fn erased_operand(&self, expr: &Expr) -> bool {
        self.shapes
            .specialization
            .instances
            .site(self.local_owner, expr)
            .erased
    }
    pub(in crate::lower) fn is_callback(&self, expr: &Expr) -> bool {
        let plans = &self.shapes.specialization.instances;
        matches!(expr, Expr::Closure { .. })
            || plans.reference(self.local_owner, expr).is_some()
            || matches!(
                plans.site(self.local_owner, expr).coercion,
                Some(FuncCoercion::Callback { .. })
            )
    }
    pub(in crate::lower) fn instance_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        required: bool,
    ) -> Result<Option<Value>> {
        let plans = &self.shapes.specialization.instances;
        let site = plans.site(self.local_owner, expr);
        let conversion = site.conversion();
        let discard = matches!(
            conversion,
            Some(ValueCoercion::Zero | ValueCoercion::Tag(..))
        );
        let input = match conversion {
            Some(ValueCoercion::Widen { from, .. }) => self.shapes.callable_type(from)?,
            Some(_) => None,
            None => expected,
        };
        let raw = if discard && crate::source::is_pure_func_expr(expr) {
            None
        } else {
            self.instance_value(
                ctx,
                expr,
                input,
                site.resolution,
                !discard && (required || conversion.is_some()),
            )?
        };
        let value = match conversion {
            None => return Ok(raw),
            Some(ValueCoercion::Zero) => self.number(ctx, 0.0),
            Some(ValueCoercion::Tag(id, target)) => {
                let target = expected
                    .map(Ok)
                    .unwrap_or_else(|| self.shapes.resolved_binding(target).map(|b| b.carrier))?;
                self.function_value(ctx, id, None, Some(target))?
            }
            Some(ValueCoercion::BoxEnv(id, target)) => {
                let target = expected
                    .map(Ok)
                    .unwrap_or_else(|| self.shapes.resolved_binding(target).map(|b| b.carrier))?;
                let wrapper = raw.ok_or_else(|| internal!("missing closure environment"))?;
                let env = self.shapes.environment_record(id)?.into();
                let payload = self.callable_field(ctx, wrapper, "val", env)?;
                self.function_value(ctx, id, Some(payload), Some(target))?
            }
            Some(ValueCoercion::Widen { from, to }) => {
                let value = raw.ok_or_else(|| internal!("missing function to widen"))?;
                let value = match input {
                    Some(ty) => self.fit(ctx, value, ty)?,
                    None => value,
                };
                self.widen_function(ctx, value, from, to)?
            }
        };
        Ok(Some(value))
    }
    fn instance_value(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        site: Option<SiteResolution>,
        required: bool,
    ) -> Result<Option<Value>> {
        let plans = &self.shapes.specialization.instances;
        let reference = match site {
            Some(SiteResolution::Callback { target }) => Some(plans.function(target)),
            _ => match expr {
                Expr::LocalGet(id) => plans.parameter_function(self.local_owner, *id),
                _ => None,
            },
        };
        if let Some(id) = reference {
            let plan = plans
                .closure_values
                .get(&(self.local_owner, id))
                .ok_or_else(|| unsupported!("this expression form is not supported yet"))?;
            return self.closure_value(ctx, plan, expected).map(Some);
        }
        use SiteResolution::*;
        let value = match site {
            None | Some(Cell(_)) => None,
            Some(TypedArray(_)) if expected.is_some() => None,
            other => other,
        };
        let Some(site) = value else {
            return if required {
                self.source_expression(ctx, expr, expected).map(Some)
            } else {
                self.source_operand(ctx, expr, expected)
            };
        };
        let result = match (site, expr) {
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
            (MemberCall(target), _) => {
                use crate::engine::solve::MemberTarget;
                use crate::lower::classes::{ClassArguments::Expressions, ClassReceiver::*};
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
                return match target {
                    MemberTarget::Resource(id) => self.resource_call(ctx, id, receiver, args),
                    MemberTarget::Class(id) => self.solved_member(
                        ctx,
                        plans.function(id),
                        Expressions(args),
                        receiver.map_or(Static, Instance),
                    ),
                }
                .map_err(|error| self.shapes.annotate_call(error, offset));
            }
            (
                ClassNew(id),
                Expr::New {
                    args, byte_offset, ..
                },
            ) => {
                let class = &self.shapes.specialization.bodies.classes[id];
                self.class_new(
                    ctx,
                    class,
                    crate::lower::classes::ClassArguments::Expressions(args),
                )
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
            (ZeroSizedArray, Expr::Array(items, _)) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    self.discard_function_operand(ctx, item)?;
                    values.push(self.number(ctx, 0.0));
                }
                let ty = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, ty, values))
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
