//! Instance facts attached to their original expression sites.
use super::*;
use crate::engine::solve::SiteResolution;
use crate::engine::{
    CallbackOperand, CaptureOperand, Conversion, EnvironmentField, ExpressionSite, InstanceValue,
    InvocationSite,
};

impl<'h> InstancePlanner<'h, '_> {
    pub(super) fn plan_expr(&mut self, scope: &InstanceContext<'_>, expr: &'h Expr) -> Result<()> {
        let owner = self.owner(scope.inst_id);
        let key = (owner, std::ptr::from_ref(expr).addr());
        let entry = self.site_entry(scope.inst_id, expr);
        let coercion = entry.and_then(|e| e.coercion);
        let resolution = entry.and_then(|e| e.resolution);
        let conversion = self.plan_conversion(key, coercion)?;
        let mut site = ExpressionSite {
            value: InstanceValue::Source,
            conversion,
        };
        if matches!(
            site.conversion,
            Some(Conversion::Zero | Conversion::Tag(..))
        ) && is_pure_func_expr(expr)
        {
            self.plans.expressions.insert(key, site);
            return Ok(());
        }
        let mut expected_children = None;
        if let Some(resolution) = resolution {
            match resolution {
                SiteResolution::BranchResult(ty) => {
                    self.prepare_layouts(ty)?;
                    site.value = InstanceValue::Expected(ty);
                    expected_children = Some(true);
                }
                SiteResolution::Cell(_) => {}
                SiteResolution::FoldBool(value) => site.value = InstanceValue::Fold(value),
                SiteResolution::CollectionNew(ty) => {
                    self.prepare_layouts(ty)?;
                    site.value = InstanceValue::Collection(ty);
                }
                SiteResolution::ThrowTypeError => site.value = InstanceValue::ThrowTypeError,
                SiteResolution::FuncTypeOf(ty) => site.value = InstanceValue::TypeOf(ty),
                SiteResolution::Callback { target } => {
                    site.value =
                        InstanceValue::Reference(self.solved.instances[target].emitted_func_id)
                }
                SiteResolution::DirectCall {
                    target,
                    argument_types,
                    pass_receiver,
                    pass_env,
                } => {
                    site.value = InstanceValue::Invoke(InvocationSite {
                        target: self.solved.instances[target].emitted_func_id,
                        operand: if pass_env {
                            CallbackOperand::Environment
                        } else {
                            CallbackOperand::None
                        },
                        receiver: pass_receiver,
                        argument_types,
                    });
                    expected_children = Some(true);
                }
                SiteResolution::DispatchCall {
                    helper,
                    argument_types,
                    needs_recv,
                } => {
                    site.value = InstanceValue::Invoke(InvocationSite {
                        target: self.helper_function(helper)?,
                        operand: CallbackOperand::Callee,
                        receiver: needs_recv,
                        argument_types,
                    });
                    expected_children = Some(true);
                }
                SiteResolution::FuncCompare {
                    left_ty,
                    right_ty,
                    op,
                    ..
                } => {
                    let equal = matches!(op, CompareOp::Eq | CompareOp::LooseEq);
                    let left_erased = self.is_zero_sized_func(left_ty);
                    site.value = if left_erased || self.is_zero_sized_func(right_ty) {
                        let (erased, source) = if left_erased {
                            (left_ty, right_ty)
                        } else {
                            (right_ty, left_ty)
                        };
                        let TyKind::Func(identity) = self.types.kind(erased) else {
                            return Err(Diagnostic::internal("missing function identity"));
                        };
                        if self.types.func_members(&source).any(|i| i == identity) {
                            InstanceValue::Matches {
                                identity,
                                source,
                                left_erased,
                                equal,
                            }
                        } else {
                            InstanceValue::Fold(!equal)
                        }
                    } else {
                        let environments = left_ty == right_ty
                            && self
                                .types
                                .is_env_func(left_ty, &self.solved.escaping_env_templates);
                        InstanceValue::Compare {
                            left: left_ty,
                            right: right_ty,
                            equal,
                            environments,
                        }
                    };
                }
                SiteResolution::ObjectLiteral(ty) => {
                    let TyKind::Object { props, .. } = self.types.kind(ty) else {
                        return Err(Diagnostic::internal("object literal has no solved fields"));
                    };
                    let fields = self.types.object_slice(props);
                    let mut field = |i: usize, e: &Expr| {
                        if fields.get(i).is_some_and(|p| self.is_zero_sized_func(p.ty)) {
                            self.plans
                                .erased
                                .insert((owner, std::ptr::from_ref(e).addr()));
                        }
                    };
                    match expr {
                        Expr::New { args, .. } => {
                            for (i, e) in args.iter().enumerate() {
                                field(i, e);
                            }
                        }
                        Expr::Object(items) => {
                            for (i, (_, e)) in items.iter().enumerate() {
                                field(i, e);
                            }
                        }
                        _ => {}
                    }
                }
                SiteResolution::TypedArray(ty) => {
                    if !scope.has_expected {
                        self.prepare_layouts(ty)?;
                        site.value = InstanceValue::Expected(ty);
                    }
                    expected_children = Some(true);
                }
                SiteResolution::ZeroSizedArray => site.value = InstanceValue::ZeroArray,
                SiteResolution::ZeroSizedFieldSet => site.value = InstanceValue::ErasedFieldSet,
                SiteResolution::MethodCall {
                    target,
                    argument_types,
                } => {
                    site.value =
                        InstanceValue::Method(self.solved.instances[target].emitted_func_id);
                    self.mark_arguments(owner, expr, argument_types);
                    expected_children = Some(true);
                }
                SiteResolution::ClassNew {
                    class_inst,
                    argument_types,
                } => {
                    site.value = InstanceValue::Construct(class_inst);
                    self.mark_arguments(owner, expr, argument_types);
                    expected_children = Some(true);
                }
                SiteResolution::PromiseNew {
                    value_loc,
                    resolve_tid,
                    reject_tid,
                    executor,
                } => {
                    let (executor, pass_resolve, pass_reject) = executor
                        .ok_or_else(|| Diagnostic::internal("unresolved promise executor"))?;
                    site.value = InstanceValue::Promise(self.promise_constructor(
                        scope.inst_id,
                        value_loc,
                        resolve_tid,
                        reject_tid,
                        executor,
                        pass_resolve,
                        pass_reject,
                    )?);
                }
                SiteResolution::PromiseReaction {
                    helper,
                    pass_first,
                    pass_second,
                    ..
                } => {
                    let target = self.helper_function(helper)?;
                    site.value = InstanceValue::Async(crate::engine::AsyncCall::Reaction {
                        target,
                        passed: [pass_first, pass_second],
                    });
                }
                SiteResolution::TimerStart { helper } => {
                    site.value = InstanceValue::Async(crate::engine::AsyncCall::StartTimer(
                        self.helper_function(helper)?,
                    ));
                }
                SiteResolution::TimerClear { arg_ty } => {
                    site.value = InstanceValue::Async(crate::engine::AsyncCall::ClearTimer {
                        has_handle: arg_ty.is_some_and(|t| !matches!(t, Ty::VOID | Ty::NULL)),
                    });
                }
            }
        }
        if matches!(site.value, InstanceValue::Source) {
            match expr {
                Expr::PrivateGuard { kind: 0, .. } => site.value = InstanceValue::Receiver,
                Expr::LocalGet(id)
                    if let Some((_, fid)) =
                        scope.func_params.iter().find(|(local, _)| local == id) =>
                {
                    site.value = InstanceValue::Reference(*fid)
                }
                Expr::Closure { func_id, .. }
                    if let Some(tid) = self.index.template_of_func(*func_id)
                        && self.is_env_template(tid) =>
                {
                    let capture_owner = if self.index.template_captures(tid).is_empty() {
                        CaptureOwner::None
                    } else if self.solved.collapsed_closures.contains(tid.index()) {
                        CaptureOwner::Template(self.solved.instances[scope.inst_id].template)
                    } else {
                        CaptureOwner::Instance(scope.inst_id)
                    };
                    let ident = FuncIdentity {
                        template: tid,
                        capture_owner,
                    };
                    self.environment(ident)?;
                    let operands = self.plans.environments[&ident]
                        .iter()
                        .filter_map(|field| {
                            let &EnvironmentField::Capture {
                                local, ty, cell, ..
                            } = field
                            else {
                                return None;
                            };
                            Some(if cell.is_some() {
                                CaptureOperand::Cell(local)
                            } else {
                                self.capture_operand(
                                    capture_owner,
                                    CaptureOwner::Instance(scope.inst_id),
                                    local,
                                    ty,
                                )
                            })
                        })
                        .collect();
                    let owner = self.owner(scope.inst_id);
                    self.plans.closure_values.insert(
                        (owner, *func_id),
                        crate::engine::ClosureValue {
                            identity: ident,
                            operands,
                        },
                    );
                }
                _ => {}
            }
        }
        let recurse = !matches!(site.value, InstanceValue::Reference(_));
        if !matches!(site.value, InstanceValue::Source) || site.conversion.is_some() {
            self.plans.expressions.insert(key, site);
        }
        if recurse {
            self.plan_children(scope, expr, expected_children)?;
        }
        Ok(())
    }

    fn plan_conversion(
        &mut self,
        key: crate::engine::SiteKey,
        coercion: Option<FuncCoercion>,
    ) -> Result<Option<Conversion>> {
        Ok(match coercion {
            Some(FuncCoercion::Zero) => Some(Conversion::Zero),
            Some(FuncCoercion::Tag(id, ty)) => Some(Conversion::Tag(id, ty)),
            Some(FuncCoercion::BoxEnv(id, ty)) => Some(Conversion::BoxEnvironment(id, ty)),
            Some(FuncCoercion::Widen { from, to }) => Some(Conversion::Widen(from, to)),
            Some(FuncCoercion::EnvCallback { target, input_tys }) => {
                let inputs = self.types.tuple_slice(input_tys);
                let binding = self.callback_binding(
                    self.solved.instances[target].emitted_func_id,
                    CallbackOperand::Environment,
                    inputs,
                )?;
                self.plans.callbacks.insert(key, binding);
                None
            }
            Some(FuncCoercion::DispatchCallback {
                helper,
                arms,
                input_tys,
            }) => {
                let inputs = self.types.tuple_slice(input_tys);
                let count = self.solved.dispatch_arms[arms]
                    .iter()
                    .map(|&(identity, _)| self.index.view(identity.template))
                    .map(|v| {
                        if v.params.iter().any(|p| p.is_rest) {
                            inputs.len()
                        } else {
                            v.params.len()
                        }
                    })
                    .max()
                    .unwrap_or(0)
                    .min(inputs.len());
                let fid = self.helper_function(helper)?;
                let binding =
                    self.callback_binding(fid, CallbackOperand::Callee, &inputs[..count])?;
                self.plans.callbacks.insert(key, binding);
                None
            }
            None => None,
        })
    }

    fn plan_children(
        &mut self,
        scope: &InstanceContext<'_>,
        expr: &'h Expr,
        expected_children: Option<bool>,
    ) -> Result<()> {
        let owner = self.owner(scope.inst_id);
        let mut positions = match expr {
            Expr::ArraySpread(items) => Some(
                items
                    .iter()
                    .filter(|item| !matches!(item, perry_hir::ir::ArrayElement::Hole)),
            ),
            _ => None,
        };
        let mut index = 0;
        let mut result = Ok(());
        perry_hir::walker::walk_expr_children(expr, &mut |child| {
            let expected = match expr {
                Expr::Conditional { .. } => {
                    index > 0 && expected_children.unwrap_or(scope.has_expected)
                }
                Expr::Logical { .. } | Expr::Sequence(_) => {
                    expected_children.unwrap_or(scope.has_expected)
                }
                Expr::LocalSet(..) => true,
                Expr::ArraySpread(_) => {
                    positions
                        .as_mut()
                        .and_then(Iterator::next)
                        .is_some_and(|item| matches!(item, perry_hir::ir::ArrayElement::Expr(_)))
                        && expected_children.unwrap_or(false)
                }
                _ => expected_children.unwrap_or(false),
            };
            if result.is_ok()
                && !(self
                    .plans
                    .erased
                    .contains(&(owner, std::ptr::from_ref(child).addr()))
                    && is_pure_func_expr(child))
            {
                result = self.plan_expr(&scope.with_expected(expected), child);
            }
            index += 1;
        });
        result
    }

    fn mark_arguments(&mut self, owner: Option<FuncId>, expr: &Expr, argument_types: Ty) {
        let args = match expr {
            Expr::Call { args, .. }
            | Expr::New { args, .. }
            | Expr::StaticMethodCall { args, .. } => args,
            _ => return,
        };
        let types = self.types.tuple_slice(argument_types);
        for (i, arg) in args.iter().enumerate() {
            if types.get(i).is_none_or(|&ty| self.erases_argument(ty)) {
                self.plans
                    .erased
                    .insert((owner, std::ptr::from_ref(arg).addr()));
            }
        }
    }
}
