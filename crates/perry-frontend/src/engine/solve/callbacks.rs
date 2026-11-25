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
                MethodResult::Optional(crate::methods::Kind::String),
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

        let (object, method_name, args): (&'h Expr, &str, &'h [Expr]) = match expr {
            Expr::ArrayMap { array, callback } => (array, "map", std::slice::from_ref(callback)),
            Expr::ArrayFlatMap { array, callback } => {
                (array, "flatMap", std::slice::from_ref(callback))
            }
            Expr::ArrayFilter { array, callback } => {
                (array, "filter", std::slice::from_ref(callback))
            }
            Expr::ArraySome { array, callback } => (array, "some", std::slice::from_ref(callback)),
            Expr::ArrayEvery { array, callback } => {
                (array, "every", std::slice::from_ref(callback))
            }
            Expr::ArrayFind { array, callback } => (array, "find", std::slice::from_ref(callback)),
            Expr::ArrayFindIndex { array, callback } => {
                (array, "findIndex", std::slice::from_ref(callback))
            }
            Expr::ArraySort { array, comparator } => {
                (array, "sort", std::slice::from_ref(comparator))
            }
            Expr::ArrayToSorted {
                array,
                comparator: Some(c),
                ..
            } => (array, "toSorted", std::slice::from_ref(c)),
            Expr::Call { callee, args, .. }
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref() =>
            {
                (object, hir_compat::method(property).name(), args.as_slice())
            }
            _ => return Ok(None),
        };
        let raw_obj = self.eval_expr(s, object)?;
        if raw_obj == Ty::BOTTOM {
            return Ok(Some(Err(Ty::BOTTOM)));
        }
        let (raw_stripped, _) = self.types.strip_nullish(raw_obj);

        if method_name == "forEach"
            && let TyKind::Generic { base, args: gargs } = self.types.kind(raw_stripped)
            && let Some(cb) = args.first()
        {
            let inputs = match (base.get(self.ctx).as_str(), self.types.ty_slice(gargs)) {
                ("Map" | "ReadonlyMap", [k, v]) => vec![*v, *k, raw_stripped],
                ("Set" | "ReadonlySet", [v]) => vec![*v, *v, raw_stripped],
                _ => return Ok(None),
            };
            return Ok(Some(Ok(CallbackSite::new(
                cb,
                inputs,
                MethodResult::Void,
                raw_obj,
            ))));
        }

        let Some(method) = builtins::receiver_of(self.ctx, self.types, raw_stripped)
            .and_then(|r| Method::lookup(r, method_name))
        else {
            return Ok(None);
        };
        let Some(cb_kind) = method.callback() else {
            return Ok(None);
        };
        let obj_ty = self.resolve_cell_read(s.inst, raw_obj);
        if obj_ty == Ty::BOTTOM {
            return Ok(Some(Err(Ty::BOTTOM)));
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        let Some((first_arg, tail)) = args.split_first() else {
            return Ok(None);
        };

        if matches!(cb_kind, Cb::Replacement) {
            let Some(cb) = tail.first() else {
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
                Expr::RegExp { pattern, .. } => Some(crate::methods::regex_captures(pattern)),
                Expr::LocalGet(id) => self
                    .index
                    .local_idx(*id)
                    .and_then(|li| self.index.local_regex_pattern(li))
                    .map(crate::methods::regex_captures),
                Expr::String(_) => Some((0, false)),
                _ => None,
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
        let (inputs, init_ty) = if matches!(cb_kind, Cb::Fold) {
            let acc = match tail.first() {
                Some(init) => self.eval_expr(s, init)?,
                None => elem,
            };
            if acc == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            (vec![acc, elem, Ty::NUMBER, stripped], Some(acc))
        } else {
            (self.array_cb_inputs(cb_kind, stripped, elem), None)
        };
        Ok(Some(Ok(CallbackSite {
            callback: first_arg,
            inputs,
            result: CallbackResult::Method(
                method.result_rule(),
                MethodResultArgs {
                    arg0: init_ty,
                    sparse_arrays: self.metadata.source.sparse_arrays,
                    ..MethodResultArgs::receiver_only(obj_ty)
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
            CallbackResult::Method(rule, args) => builtins::apply_method_result(
                self.ctx,
                self.types,
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
            let capture_owner = if self.index.template_captures(template).is_empty() {
                CaptureOwner::None
            } else if self.collapsed_closures.contains(template.index())
                && let Some(owner_tid) = self.instances.get(s.inst).map(|i| i.template)
            {
                CaptureOwner::Template(owner_tid)
            } else {
                CaptureOwner::Instance(s.inst)
            };
            FuncIdentity {
                template,
                capture_owner,
            }
        } else {
            let cb_val = self.eval_expr(s, callback)?;
            if cb_val == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            if let Some(idents) = self.types.func_dispatch_members(cb_val, false) {
                let mut arms = Vec::with_capacity(idents.len());
                let mut cb_ret = Ty::BOTTOM;
                let mut widest_inputs: &[Ty] = &[];
                for ident in idents {
                    let cb_view = self.index.view(ident.template);
                    let arm_inputs = cb_view.callback_inputs(inputs);
                    if arm_inputs.len() > widest_inputs.len() {
                        widest_inputs = arm_inputs;
                    }
                    let cb_inst = self.request_instance(
                        InstanceReq::new(ident.template, arm_inputs, Some(s.inst))
                            .owner(ident.capture_owner),
                    )?;
                    let r = self.read_loc(self.instances[cb_inst].return_loc, s.inst);
                    cb_ret = self.types.join(cb_ret, r);
                    arms.push((ident, cb_inst));
                }
                let arms_span = self.intern_dispatch_arms(&arms);
                let input_tys = self.types.tuple(widest_inputs);
                let helper = self.intern_helper(
                    HelperSpec::Dispatcher {
                        result: cb_ret,
                        callee_ty: cb_val,
                        arms: arms_span,
                        arg_tys: input_tys,
                        recv_ty: None,
                    },
                    0,
                );
                self.record_site_coercion(
                    s.inst,
                    callback,
                    FuncCoercion::DispatchCallback {
                        helper,
                        arms: arms_span,
                        input_tys,
                    },
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
                    FuncCoercion::EnvCallback {
                        target: cb_inst,
                        input_tys,
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
