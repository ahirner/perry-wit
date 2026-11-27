//! Contextual typing for built-in callbacks.
use super::calls::CallOrigin;
use super::*;
use crate::engine::boundary::TemplateView;
use crate::methods::{Callback as Cb, Method, MethodResult};

#[derive(Clone, Copy)]
pub(super) enum DispatchSite<'h> {
    Call(Option<Ty>, CallOrigin<'h>),
    Builtin,
}

impl TemplateView<'_> {
    pub(super) fn accepted_inputs<'i>(&self, inputs: &'i [Ty]) -> &'i [Ty] {
        if self.params.iter().any(|p| p.is_rest) {
            inputs
        } else {
            inputs.get(..self.params.len()).unwrap_or(inputs)
        }
    }
}

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn try_eval_callback_intrinsic(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
    ) -> Result<Option<Ty>> {
        if let Expr::JsonStringifyFull(value, callback, space) = expr
            && matches!(callback.as_ref(), Expr::Closure { .. } | Expr::FuncRef(_))
        {
            let receiver = self.eval_expr(s, value)?;
            self.eval_expr(s, space)?;
            return self.eval_method_callback(
                s,
                expr,
                callback,
                &[Ty::STRING, Ty::DYNAMIC],
                MethodResult::Signature(&[perry_mir::SignatureType::OptString]),
                MethodResultArgs::receiver_only(receiver),
            );
        }
        if let Some((items, key_fn)) = hir_compat::map_group_by(expr)? {
            let src = self.eval_expr(s, items)?;
            if src == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            let elem = builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN);
            if elem == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            return Ok(self
                .contextual_callback(s, expr, key_fn, &[elem, Ty::NUMBER])?
                .map(|key| {
                    if key == Ty::BOTTOM {
                        return Ty::BOTTOM;
                    }
                    let values = self.types.array(elem);
                    self.types.generic(
                        shapes::TypeName::new(self.ctx, "Map".into()),
                        &[key, values],
                    )
                }));
        }

        if let Expr::ArrayFromMapped {
            iterable, map_fn, ..
        } = expr
        {
            let src = self.eval_expr(s, iterable)?;
            if src == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            let len_name = self.types.intern_name("length");
            let src_def = self.types.strip_nullish(src).0;
            let is_array_like = match self.types.kind(src_def) {
                TyKind::Object { props, .. } => self
                    .types
                    .object_slice(props)
                    .iter()
                    .any(|p| p.name == len_name),
                _ => {
                    matches!(iterable.as_ref(), Expr::Object(p) if p.iter().any(|(k, _)| k == "length"))
                }
            };
            let elem = if is_array_like {
                Ty::VOID
            } else {
                builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN)
            };
            return self.eval_method_callback(
                s,
                expr,
                map_fn,
                &[elem, Ty::NUMBER],
                MethodResult::Mapped,
                MethodResultArgs::receiver_only(src),
            );
        }

        Ok(None)
    }

    pub(super) fn eval_callback_method(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        raw_obj: Ty,
        method: Method,
        first_arg: &'h Expr,
        second_arg: Option<&'h Expr>,
    ) -> Result<Option<Ty>> {
        let Some(cb_kind) = method.callback() else {
            return Ok(None);
        };
        let obj_ty = self.resolve_cell_read(s.inst, raw_obj);
        if obj_ty == Ty::BOTTOM {
            return Ok(Some(Ty::BOTTOM));
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        let mut result_args = MethodResultArgs::receiver_only(raw_obj);
        let mut replacement = Vec::new();
        let (callback, inputs): (_, &[Ty]) = if matches!(cb_kind, Cb::Collection) {
            let TyKind::Generic { base, args } = self.types.kind(stripped) else {
                return Ok(None);
            };
            let Some([key, value]) =
                crate::source::CollectionKind::from_type_name(base.get(self.ctx))
                    .and_then(|kind| kind.type_fields(self.types.ty_slice(args)))
            else {
                return Ok(None);
            };
            (first_arg, &[value, key, stripped][..])
        } else if matches!(cb_kind, Cb::Replacement) {
            let Some(cb) = second_arg else {
                return Ok(None);
            };
            let search = self.eval_expr(s, first_arg)?;
            let search = self.search_pattern(s.inst, first_arg, search);
            let cb_ty = self.eval_expr(s, cb)?;
            if search == TyKind::Bottom || cb_ty == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            if !matches!(self.types.kind(cb_ty), TyKind::Func(_)) {
                return Ok(None);
            }
            replacement.push(Ty::STRING);
            let captures = match search {
                TyKind::String | TyKind::Literal(_) => Some((0, false)),
                TyKind::Regex(shape) => shape.captures,
                _ => None,
            };
            if let Some((count, named)) = captures {
                let opt = self.types.with_absences(Ty::STRING, Absence::Undefined);
                replacement.extend(std::iter::repeat_n(opt, count as usize));
                replacement.extend([Ty::NUMBER, Ty::STRING]);
                if named {
                    replacement.push(self.types.object(&[], Some(opt)));
                }
            }
            (cb, replacement.as_slice())
        } else {
            if matches!(cb_kind, Cb::Comparator) && matches!(first_arg, Expr::Undefined) {
                return Ok(None);
            }
            let Some(elem) = builtins::iterated_element(self.ctx, self.types, stripped) else {
                return Ok(None);
            };
            let input_array = if self.arrays.contains(self.types, raw_obj) {
                raw_obj
            } else {
                stripped
            };
            result_args.sparse_arrays = self.hir.sparse_arrays;
            if matches!(cb_kind, Cb::Fold) {
                let acc = match second_arg {
                    Some(init) => self.eval_expr(s, init)?,
                    None => elem,
                };
                if acc == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                result_args.arg0 = Some(acc);
                (first_arg, &[acc, elem, Ty::NUMBER, input_array][..])
            } else if matches!(cb_kind, Cb::Comparator) {
                let elem = self.types.exclude_absences(elem, Absence::Undefined);
                (first_arg, &[elem, elem][..])
            } else {
                let elem = if matches!(cb_kind, Cb::AllIndices) {
                    self.types.with_absences(elem, Absence::Undefined)
                } else {
                    elem
                };
                (first_arg, &[elem, Ty::NUMBER, input_array][..])
            }
        };
        self.eval_method_callback(s, expr, callback, inputs, method.result_rule(), result_args)
    }

    fn eval_method_callback(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callback: &'h Expr,
        inputs: &[Ty],
        rule: MethodResult,
        call: MethodResultArgs<'_>,
    ) -> Result<Option<Ty>> {
        if inputs.contains(&Ty::BOTTOM) {
            return Ok(Some(Ty::BOTTOM));
        }
        Ok(self
            .contextual_callback(s, expr, callback, inputs)?
            .map(|result| {
                self.method_result(
                    s.inst,
                    rule,
                    MethodResultArgs {
                        callback_result: Some(result),
                        ..call
                    },
                )
            }))
    }

    pub(super) fn select_call_arm(
        &mut self,
        s: &mut EvalState<'h>,
        identity: FuncIdentity,
        recv_ty: Option<Ty>,
        arg_tys: &[Ty],
        invocation: CallOrigin<'h>,
    ) -> Result<(DispatchArm, Ty, usize)> {
        let byte_offset = invocation.offset();
        let (source, type_args) = match invocation {
            CallOrigin::Source(call) => (Some(call), call.type_args),
            CallOrigin::Callback(_, type_args) => (None, type_args),
        };
        let args = source.and_then(|call| call.args.plain());
        let view = self.index.view(identity.template);
        let is_promise_handle =
            self.apply_promise_handle_call(s, identity, arg_tys, args, byte_offset)?;
        if recv_ty.is_none()
            && args.into_iter().flatten().any(|a| {
                matches!(
                    a,
                    Expr::Closure {
                        captures_new_target: true,
                        ..
                    }
                )
            })
        {
            return Err(self.hir.locations.annotate_call(
                Diagnostic::unsupported(
                    "a callback that captures new.target requires its original receiver",
                ),
                byte_offset,
            ));
        }
        if args.is_some()
            && recv_ty.is_none()
            && !is_promise_handle
            && !view.params.iter().any(|p| p.is_rest)
            && arg_tys.len() > view.params.len()
        {
            self.check_escaping_unannotated_func(identity.template, None)?;
        }
        self.check_callback_params(s.inst, view, arg_tys, byte_offset)?;
        let recv = recv_ty
            .filter(|_| view.uses_this && !view.captures_this)
            .map(Receiver::Object);
        let call_args = if is_promise_handle {
            arg_tys.get(..1).unwrap_or(&[])
        } else if source.is_some() && !view.kind.is_class_member() {
            arg_tys
        } else {
            view.accepted_inputs(arg_tys)
        };
        let type_args = self.eval_type_args(s.inst, type_args);
        let target = self.request_instance(
            InstanceReq::new(identity.template, call_args, Some(s.inst))
                .receiver(recv)
                .owner(identity.capture_owner)
                .type_args(&type_args)
                .at(byte_offset),
        )?;
        self.propagate_writes(s.inst, target);
        let ret = self.read_loc(self.instances[target].return_loc, s.inst);
        Ok((
            DispatchArm {
                identity,
                target,
                receiver: recv.is_some(),
            },
            ret,
            call_args.len(),
        ))
    }

    pub(super) fn callback_dispatcher(
        &mut self,
        s: &mut EvalState<'h>,
        callee_ty: Ty,
        identities: &[FuncIdentity],
        inputs: &[Ty],
        site: DispatchSite<'h>,
    ) -> Result<(HelperId, bool, Ty)> {
        let (receiver, origin, mut width) = match site {
            DispatchSite::Call(receiver, CallOrigin::Source(call)) => (
                receiver,
                CallOrigin::Callback(call.offset, call.type_args),
                inputs.len(),
            ),
            DispatchSite::Call(receiver, callback) => (receiver, callback, inputs.len()),
            DispatchSite::Builtin => (None, CallOrigin::Callback(0, &[]), 0),
        };
        let mut result = Ty::BOTTOM;
        let mut needs_recv = false;
        let mut arms = Vec::with_capacity(identities.len());
        for &input in inputs {
            self.invalidate_conservative_cell(input);
        }
        for &identity in identities {
            let (arm, ret, count) = self.select_call_arm(s, identity, receiver, inputs, origin)?;
            width = width.max(count);
            result = self.types.join(result, ret);
            needs_recv |= arm.receiver;
            arms.push(arm);
        }
        let arms = self.intern_dispatch_arms(&arms);
        let arg_tys = self.types.tuple(&inputs[..width]);
        let helper = self.intern_helper(
            HelperSpec::Dispatcher {
                result,
                callee_ty,
                arms,
                arg_tys,
                recv_ty: receiver.filter(|_| needs_recv),
            },
            origin.offset(),
        );
        Ok((helper, needs_recv, result))
    }

    fn contextual_callback(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callback: &'h Expr,
        inputs: &[Ty],
    ) -> Result<Option<Ty>> {
        let literal = matches!(callback, Expr::Closure { .. });
        let ident = if let Expr::Closure { func_id, .. } = callback {
            let Some(template) = self.index.template_of_func(*func_id) else {
                return Ok(None);
            };
            self.closure_identity(s.inst, template)
        } else {
            let value = self.eval_expr(s, callback)?;
            if value == Ty::BOTTOM {
                return Ok(Some(value));
            }
            if let Some(idents) = self.types.func_dispatch_members(value, false) {
                let (helper, _, ret) =
                    self.callback_dispatcher(s, value, &idents, inputs, DispatchSite::Builtin)?;
                self.record_operand_use(
                    s.inst,
                    callback,
                    OperandUse::Callback(FunctionTarget::Dispatcher(helper)),
                );
                return Ok(Some(ret));
            }
            let TyKind::Func(ident) = self.types.kind(value) else {
                return Ok(None);
            };
            ident
        };
        let cb_view = self.index.view(ident.template);
        let caller_tid = self.instances[s.inst].template;
        let caller_is_env = self.is_escaping_env(caller_tid, s.inst);
        let keep_inline = literal
            && (matches!(expr, Expr::JsonStringifyFull(..))
                || cb_view.captures_this
                || cb_view.captures_new_target
                || (caller_is_env && !self.index.template_captures(ident.template).is_empty()));
        let inputs = cb_view.accepted_inputs(inputs);
        let pass_env = !literal && self.is_escaping_env(ident.template, s.inst);
        let mut request =
            InstanceReq::new(ident.template, inputs, Some(s.inst)).owner(ident.capture_owner);
        if !pass_env {
            request = request
                .receiver(keep_inline.then_some(s.receiver).flatten())
                .callback(keep_inline);
        }
        let target = self.request_instance(request)?;
        if pass_env {
            self.record_operand_use(
                s.inst,
                callback,
                OperandUse::Callback(FunctionTarget::Closure(target)),
            );
        } else {
            self.record_site(s.inst, callback, SiteResolution::Callback { target });
        }
        Ok(Some(
            self.read_loc(self.instances[target].return_loc, s.inst),
        ))
    }
}
