//! Contextual typing for built-in callbacks.
use super::*;
use crate::engine::boundary::TemplateView;
use crate::methods::{Callback as Cb, Method, MethodResult};

struct CallbackSite<'h> {
    callback: &'h Expr,
    inputs: Vec<Ty>,
    result: CallbackResult,
}

enum CallbackResult {
    Method(MethodResult, MethodResultArgs<'static>),
    GroupBy(Ty),
}

impl<'h> CallbackSite<'h> {
    fn new(callback: &'h Expr, inputs: Vec<Ty>, rule: MethodResult, recv_ty: Ty) -> Self {
        Self {
            callback,
            inputs,
            result: CallbackResult::Method(rule, MethodResultArgs::receiver_only(recv_ty)),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum DispatchSite {
    Call(Option<Ty>, u32),
    Builtin,
}

impl TemplateView<'_> {
    fn callback_inputs<'i>(&self, inputs: &'i [Ty]) -> &'i [Ty] {
        if self.params.iter().any(|p| p.is_rest) {
            inputs
        } else {
            inputs.get(..self.params.len()).unwrap_or(inputs)
        }
    }
}

impl<'h, 'a> Solver<'h, 'a> {
    fn array_cb_inputs(&mut self, kind: Cb, recv_ty: Ty, elem: Ty) -> Vec<Ty> {
        match kind {
            Cb::Elements => vec![elem, Ty::NUMBER, recv_ty],
            Cb::AllIndices => vec![
                self.types.with_absences(elem, Absence::Undefined),
                Ty::NUMBER,
                recv_ty,
            ],
            Cb::Comparator => {
                let d = self.types.exclude_absences(elem, Absence::Undefined);
                vec![d, d]
            }
            _ => Vec::new(),
        }
    }

    fn match_builtin_callback(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
    ) -> Result<Option<Result<CallbackSite<'h>, Ty>>> {
        if let Expr::JsonStringifyFull(value, callback, space) = expr
            && matches!(callback.as_ref(), Expr::Closure { .. } | Expr::FuncRef(_))
        {
            let receiver = self.eval_expr(s, value)?;
            self.eval_expr(s, space)?;
            return Ok(Some(Ok(CallbackSite::new(
                callback,
                vec![Ty::STRING, Ty::DYNAMIC],
                MethodResult::Signature(&[perry_mir::SignatureType::OptString]),
                receiver,
            ))));
        }
        let group_by = match expr {
            Expr::MapGroupBy { items, key_fn } => Some((items.as_ref(), key_fn.as_ref())),
            Expr::Call { callee, args, .. }
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                    && let Expr::PropertyGet {
                        object: g,
                        property: cls,
                        ..
                    } = object.as_ref()
                    && matches!(g.as_ref(), Expr::GlobalGet(_))
                    && cls == "Map"
                    && hir_compat::method(property).name() == "groupBy"
                    && let [items, key_fn] = args.as_slice() =>
            {
                Some((items, key_fn))
            }
            _ => None,
        };
        if let Some((items, key_fn)) = group_by {
            let src = self.eval_expr(s, items)?;
            if src == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            let elem = builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN);
            return Ok(Some(Ok(CallbackSite {
                callback: key_fn,
                inputs: vec![elem, Ty::NUMBER],
                result: CallbackResult::GroupBy(elem),
            })));
        }

        if let Expr::ArrayFromMapped {
            iterable, map_fn, ..
        } = expr
        {
            let src = self.eval_expr(s, iterable)?;
            if src == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
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
            return Ok(Some(Ok(CallbackSite::new(
                map_fn.as_ref(),
                vec![elem, Ty::NUMBER],
                MethodResult::Mapped,
                src,
            ))));
        }

        let (raw_obj, method, first_arg, second_arg) =
            if let Some((array, method, cb, init)) = crate::methods::Call::array_callback(expr) {
                (self.eval_expr(s, array)?, method, cb, init)
            } else if let Expr::Call { callee, args, .. } = expr
                && let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                && let Some(first) = args.first()
            {
                let name = hir_compat::method(property);
                let raw_obj = self.eval_expr(s, object)?;
                if raw_obj == Ty::BOTTOM {
                    return Ok(Some(Err(Ty::BOTTOM)));
                }
                let (raw_stripped, _) = self.types.strip_nullish(raw_obj);
                let Some(method) =
                    builtins::receiver_of(self.ctx, self.decls, self.types, raw_stripped)
                        .and_then(|r| Method::lookup(r, name.name()))
                else {
                    return Ok(None);
                };
                if method.callback().is_some()
                    || matches!(method, Method::MapForEach | Method::SetForEach)
                {
                    self.record_site(s.inst, expr, SiteResolution::Builtin(method));
                }
                if let TyKind::Generic { args, .. } = self.types.kind(raw_stripped) {
                    let inputs = match (method, self.types.ty_slice(args)) {
                        (Method::MapForEach, [k, v]) => Some(vec![*v, *k, raw_stripped]),
                        (Method::SetForEach, [v]) => Some(vec![*v, *v, raw_stripped]),
                        _ => None,
                    };
                    if let Some(inputs) = inputs {
                        return Ok(Some(Ok(CallbackSite::new(
                            first,
                            inputs,
                            method.result_rule(),
                            raw_obj,
                        ))));
                    }
                }
                (raw_obj, method, first, args.get(1))
            } else {
                return Ok(None);
            };
        if raw_obj == Ty::BOTTOM {
            return Ok(Some(Err(Ty::BOTTOM)));
        }
        let Some(cb_kind) = method.callback() else {
            return Ok(None);
        };
        let obj_ty = self.resolve_cell_read(s.inst, raw_obj);
        if obj_ty == Ty::BOTTOM {
            return Ok(Some(Err(Ty::BOTTOM)));
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        if matches!(cb_kind, Cb::Replacement) {
            let Some(cb) = second_arg else {
                return Ok(None);
            };
            let cb_ty = self.eval_expr(s, cb)?;
            if cb_ty == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            if !matches!(self.types.kind(cb_ty), TyKind::Func(_)) {
                return Ok(None);
            }
            let mut inputs = vec![Ty::STRING];
            let captures = match first_arg {
                Expr::String(_) => Some((0, false)),
                _ => self.index.regexes.get(first_arg).map(|r| r.captures),
            };
            if let Some((count, named)) = captures {
                let opt = self.types.with_absences(Ty::STRING, Absence::Undefined);
                inputs.extend(std::iter::repeat_n(opt, count));
                inputs.extend([Ty::NUMBER, Ty::STRING]);
                if named {
                    inputs.push(self.types.object(&[], Some(opt)));
                }
            }
            return Ok(Some(Ok(CallbackSite::new(
                cb,
                inputs,
                method.result_rule(),
                obj_ty,
            ))));
        }

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
        let (inputs, init_ty) = if matches!(cb_kind, Cb::Fold) {
            let acc = match second_arg {
                Some(init) => self.eval_expr(s, init)?,
                None => elem,
            };
            if acc == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            (vec![acc, elem, Ty::NUMBER, input_array], Some(acc))
        } else {
            (self.array_cb_inputs(cb_kind, input_array, elem), None)
        };
        Ok(Some(Ok(CallbackSite {
            callback: first_arg,
            inputs,
            result: CallbackResult::Method(
                method.result_rule(),
                MethodResultArgs {
                    arg0: init_ty,
                    sparse_arrays: self.hir.sparse_arrays,
                    ..MethodResultArgs::receiver_only(raw_obj)
                },
            ),
        })))
    }

    pub(super) fn try_eval_builtin_callback(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
    ) -> Result<Option<Ty>> {
        let Some(matched) = self.match_builtin_callback(s, expr)? else {
            return Ok(None);
        };
        let site = match matched {
            Ok(site) => site,
            Err(early) => return Ok(Some(early)),
        };
        if site.inputs.contains(&Ty::BOTTOM) {
            return Ok(Some(Ty::BOTTOM));
        }
        let Some(result) = self.contextual_callback(s, expr, site.callback, &site.inputs)? else {
            return Ok(None);
        };
        Ok(Some(match site.result {
            CallbackResult::Method(rule, args) => self.method_result(
                s.inst,
                rule,
                MethodResultArgs {
                    callback_result: Some(result),
                    ..args
                },
            ),
            CallbackResult::GroupBy(_) if result == Ty::BOTTOM => Ty::BOTTOM,
            CallbackResult::GroupBy(element) => {
                let values = self.types.array(element);
                self.types.generic(
                    shapes::TypeName::new(self.ctx, "Map".into()),
                    &[result, values],
                )
            }
        }))
    }

    pub(super) fn callback_target(
        &mut self,
        s: &mut EvalState<'h>,
        identity: FuncIdentity,
        recv_ty: Option<Ty>,
        arg_tys: &[Ty],
        byte_offset: u32,
    ) -> Result<(DispatchArm, Ty, usize)> {
        let view = self.index.view(identity.template);
        let is_promise_handle =
            self.apply_promise_handle_call(s, identity, arg_tys, None, byte_offset)?;
        self.check_callback_params(s.inst, view, arg_tys, byte_offset)?;
        let recv = recv_ty.filter(|_| view.uses_this).map(Receiver::Object);
        let call_args = if is_promise_handle {
            arg_tys.get(..1).unwrap_or(&[])
        } else {
            view.callback_inputs(arg_tys)
        };
        let target = self.request_instance(
            InstanceReq::new(identity.template, call_args, Some(s.inst))
                .receiver(recv)
                .owner(identity.capture_owner)
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
        site: DispatchSite,
    ) -> Result<(CallTarget, Ty, Ty)> {
        let (receiver, byte_offset, mut width) = match site {
            DispatchSite::Call(receiver, byte_offset) => (receiver, byte_offset, inputs.len()),
            DispatchSite::Builtin => (None, 0, 0),
        };
        let mut result = Ty::BOTTOM;
        let mut needs_recv = false;
        let mut arms = Vec::with_capacity(identities.len());
        for &input in inputs {
            self.invalidate_conservative_cell(input);
        }
        for &identity in identities {
            let (arm, ret, count) =
                self.callback_target(s, identity, receiver, inputs, byte_offset)?;
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
            byte_offset,
        );
        Ok((CallTarget::Dispatch { helper, needs_recv }, arg_tys, result))
    }

    fn contextual_callback(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callback: &'h Expr,
        inputs: &[Ty],
    ) -> Result<Option<Ty>> {
        let ident = if let Expr::Closure { func_id, .. } = callback {
            let Some(template) = self.index.template_of_func(*func_id) else {
                return Ok(None);
            };
            self.closure_identity(s.inst, template)
        } else {
            let cb_val = self.eval_expr(s, callback)?;
            if cb_val == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            if let Some(idents) = self.types.func_dispatch_members(cb_val, false) {
                let (target, inputs, cb_ret) =
                    self.callback_dispatcher(s, cb_val, &idents, inputs, DispatchSite::Builtin)?;
                self.record_site_coercion(
                    s.inst,
                    callback,
                    FuncCoercion::Callback { target, inputs },
                );
                return Ok(Some(cb_ret));
            }
            let TyKind::Func(ident) = self.types.kind(cb_val) else {
                return Ok(None);
            };
            if self.is_escaping_env(ident.template, s.inst) {
                let cb_view = self.index.view(ident.template);
                let inputs = cb_view.callback_inputs(inputs);
                let cb_inst = self.request_instance(
                    InstanceReq::new(ident.template, inputs, Some(s.inst))
                        .owner(ident.capture_owner),
                )?;
                let input_tys = self.types.tuple(inputs);
                self.record_site_coercion(
                    s.inst,
                    callback,
                    FuncCoercion::Callback {
                        target: CallTarget::Direct {
                            target: cb_inst,
                            pass_env: true,
                            pass_receiver: false,
                        },
                        inputs: input_tys,
                    },
                );
                return Ok(Some(
                    self.read_loc(self.instances[cb_inst].return_loc, s.inst),
                ));
            }
            ident
        };
        let cb_view = self.index.view(ident.template);
        let caller_tid = self.instances[s.inst].template;
        let caller_is_env = self.is_escaping_env(caller_tid, s.inst);
        let keep_inline = matches!(callback, Expr::Closure { .. })
            && (matches!(expr, Expr::JsonStringifyFull(..))
                || cb_view.captures_this
                || cb_view.captures_new_target
                || (caller_is_env && !self.index.template_captures(ident.template).is_empty()));
        let inputs = cb_view.callback_inputs(inputs);
        let cb_inst = self.request_instance(
            InstanceReq::new(ident.template, inputs, Some(s.inst))
                .receiver(keep_inline.then_some(s.receiver).flatten())
                .owner(ident.capture_owner)
                .callback(keep_inline),
        )?;
        self.record_site(
            s.inst,
            callback,
            SiteResolution::Callback { target: cb_inst },
        );
        let Some(cb_ret_loc) = self.instances.get(cb_inst).map(|i| i.return_loc) else {
            return Ok(None);
        };
        Ok(Some(self.read_loc(cb_ret_loc, s.inst)))
    }
}
