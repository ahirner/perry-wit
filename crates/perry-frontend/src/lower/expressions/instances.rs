//! Lower a solved operation at its lexical expression.
use super::*;
use crate::engine::{Conversion, ExpressionSite, InstanceValue};

impl<'s> FunctionBuilder<'s> {
    pub(in crate::lower) fn erased_operand(&self, expr: &Expr) -> bool {
        self.shapes
            .specialization
            .instances
            .erased
            .contains(&(self.local_owner, std::ptr::from_ref(expr).addr()))
    }
    pub(in crate::lower) fn is_callback(&self, expr: &Expr) -> bool {
        let plans = &self.shapes.specialization.instances;
        matches!(expr, Expr::Closure { .. })
            || plans.reference(self.local_owner, expr).is_some()
            || plans
                .callbacks
                .contains_key(&(self.local_owner, std::ptr::from_ref(expr).addr()))
    }
    pub(in crate::lower) fn instance_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        site: ExpressionSite,
        required: bool,
    ) -> Result<Option<Value>> {
        let discard = matches!(
            site.conversion,
            Some(Conversion::Zero | Conversion::Tag(..))
        );
        let input = match site.conversion {
            Some(Conversion::Widen(from, _)) => self.shapes.callable_type(ctx, from)?,
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
                site.value,
                !discard && (required || site.conversion.is_some()),
            )?
        };
        let value = match site.conversion {
            None => return Ok(raw),
            Some(Conversion::Zero) => self.number(ctx, 0.0),
            Some(Conversion::Tag(id, target)) => {
                let target = expected.map(Ok).unwrap_or_else(|| {
                    self.shapes.resolved_binding(ctx, target).map(|b| b.carrier)
                })?;
                self.function_value(ctx, id, None, Some(target))?
            }
            Some(Conversion::BoxEnvironment(id, target)) => {
                let target = expected.map(Ok).unwrap_or_else(|| {
                    self.shapes.resolved_binding(ctx, target).map(|b| b.carrier)
                })?;
                let wrapper = raw.ok_or_else(|| internal!("missing closure environment"))?;
                let env = self.shapes.environment_type(ctx, id)?;
                let payload = self.callable_field(ctx, wrapper, "val", env)?;
                self.function_value(ctx, id, Some(payload), Some(target))?
            }
            Some(Conversion::Widen(from, to)) => {
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
        site: InstanceValue,
        required: bool,
    ) -> Result<Option<Value>> {
        let result = match (site, expr) {
            (InstanceValue::Source, _) => {
                return if required {
                    self.source_expression(ctx, expr, expected).map(Some)
                } else {
                    self.source_operand(ctx, expr, expected)
                };
            }
            (InstanceValue::Expected(ty), _) => {
                let ty = match expected {
                    Some(ty) if crate::lower::value_unions::is_value_union(ctx, ty) => ty,
                    _ => self.shapes.resolved_binding(ctx, ty)?.carrier,
                };
                self.source_expression(ctx, expr, Some(ty))?
            }
            (InstanceValue::Reference(id), _) => {
                if let Some(plan) = self
                    .shapes
                    .specialization
                    .instances
                    .closure_values
                    .get(&(self.local_owner, id))
                {
                    self.closure_value(ctx, plan, expected)?
                } else {
                    bail_unsupported!("this expression form is not supported yet")
                }
            }
            (InstanceValue::Receiver, Expr::PrivateGuard { object, .. }) => {
                self.expression_with(ctx, object, expected)?
            }
            (
                InstanceValue::Invoke(site),
                Expr::Call {
                    callee,
                    args,
                    byte_offset,
                    ..
                },
            ) => {
                return self
                    .invoke_site(ctx, site, callee, args)
                    .map_err(|e| self.shapes.annotate_call(e, *byte_offset));
            }
            (InstanceValue::Promise(plan), Expr::New { args, .. }) => {
                self.new_promise(ctx, plan, args)?
            }
            (InstanceValue::Collection(ty), _) => {
                let c = self
                    .shapes
                    .source
                    .constructors
                    .collection(expr)
                    .ok_or_else(|| internal!("missing collection constructor"))?;
                let ty = self.shapes.resolved_binding(ctx, ty)?.carrier;
                self.new_collection(
                    ctx,
                    c.kind == crate::source::CollectionKind::Map,
                    c.init,
                    Some(ty),
                )?
            }
            (
                InstanceValue::Async(plan),
                Expr::Call {
                    callee,
                    args,
                    byte_offset,
                    ..
                },
            ) => {
                let value = self
                    .async_call(ctx, plan, callee, args)
                    .map_err(|e| self.shapes.annotate_call(e, *byte_offset))?;
                if required
                    && value.is_none()
                    && matches!(plan, crate::engine::AsyncCall::ClearTimer { .. })
                {
                    self.absent_literal(ctx, false, expected)?
                } else {
                    return Ok(value);
                }
            }
            (InstanceValue::Invoke(site), Expr::CallSpread { args, .. }) => {
                self.func_call_spread(ctx, site.target, args)?
            }
            (InstanceValue::Method(id), _) => {
                return self
                    .instance_method_call(ctx, expr, id)
                    .map_err(|error| match expr {
                        Expr::Call { byte_offset, .. } => {
                            self.shapes.annotate_call(error, *byte_offset)
                        }
                        _ => error,
                    });
            }
            (
                InstanceValue::Construct(id),
                Expr::New {
                    args, byte_offset, ..
                },
            ) => {
                let name = &self.shapes.specialization.class_names[id];
                self.class_new(ctx, name, args)
                    .map_err(|e| self.shapes.annotate_call(e, *byte_offset))?
            }
            (InstanceValue::ThrowTypeError, _) => {
                if let Expr::Call { callee, args, .. } = expr {
                    self.discard_function_operand(ctx, callee)?;
                    for arg in args {
                        self.discard_function_operand(ctx, arg)?;
                    }
                }
                self.raise_non_callable(ctx)
            }
            (
                InstanceValue::ErasedFieldSet,
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
            (InstanceValue::ZeroArray, Expr::Array(items, _)) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    self.discard_function_operand(ctx, item)?;
                    values.push(self.number(ctx, 0.0));
                }
                let ty = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, ty, values))
            }
            (InstanceValue::Fold(value), Expr::Compare { left, right, .. }) => {
                self.discard_function_operand(ctx, left)?;
                self.discard_function_operand(ctx, right)?;
                self.boolean(ctx, value)
            }
            (InstanceValue::TypeOf(ty), Expr::TypeOf(operand)) => match ty {
                Some(ty) => self.callable_typeof(ctx, operand, ty)?,
                None => {
                    self.discard_function_operand(ctx, operand)?;
                    self.string(ctx, "function")
                }
            },
            (
                InstanceValue::Matches {
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
                InstanceValue::Compare {
                    left: left_ty,
                    right: right_ty,
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
