//! Call, method, constructor, built-in callback, and host-import resolution.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn try_eval_builtin_callback(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<Option<Ty>> {
        // Pre-evaluate receiver / iterable expressions needed by the callback table.
        let (callback, mut inputs, contract, recv_ty, map_group_elem) = match expr {
            Expr::ArrayMap { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                (
                    callback.as_ref(),
                    vec![elem, Ty::NUMBER, r],
                    CallbackContract::Mapped {
                        keep_holes: self.facts.sparse_arrays,
                    },
                    r,
                    None,
                )
            }
            Expr::ArrayFlatMap { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                (
                    callback.as_ref(),
                    vec![elem, Ty::NUMBER, r],
                    CallbackContract::FlatMapped,
                    r,
                    None,
                )
            }
            Expr::ArrayFilter { array, callback }
            | Expr::ArraySome { array, callback }
            | Expr::ArrayEvery { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                (
                    callback.as_ref(),
                    vec![elem, Ty::NUMBER, r],
                    CallbackContract::Predicate,
                    r,
                    None,
                )
            }
            Expr::ArrayFind { array, callback } | Expr::ArrayFindIndex { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                let opt = self.types.with_absences(elem, Absence::Undefined);
                (
                    callback.as_ref(),
                    vec![opt, Ty::NUMBER, r],
                    CallbackContract::Predicate,
                    r,
                    None,
                )
            }
            Expr::ArraySort { array, comparator }
                if !matches!(comparator.as_ref(), Expr::Undefined) =>
            {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                let def = self.types.exclude_absences(elem, Absence::Undefined);
                (
                    comparator.as_ref(),
                    vec![def, def],
                    CallbackContract::Comparator,
                    r,
                    None,
                )
            }
            Expr::ArrayToSorted {
                array,
                comparator: Some(comparator),
                ..
            } if !matches!(comparator.as_ref(), Expr::Undefined) => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                let def = self.types.exclude_absences(elem, Absence::Undefined);
                (
                    comparator.as_ref(),
                    vec![def, def],
                    CallbackContract::Comparator,
                    r,
                    None,
                )
            }
            Expr::ArrayFromMapped {
                iterable, map_fn, ..
            } => {
                let src = self.eval_expr(inst_id, receiver, iterable, state)?;
                if src == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let length_name = self.types.intern_name("length");
                let (src_def, _) = self.types.strip_nullish(src);
                let is_array_like = match self.types.kind(src_def) {
                    TyKind::Object { props, .. } => self
                        .types
                        .object_slice(props)
                        .iter()
                        .any(|p| p.name == length_name),
                    _ => {
                        matches!(iterable.as_ref(), Expr::Object(props) if props.iter().any(|(k, _)| k == "length"))
                    }
                };
                let elem = if is_array_like {
                    Ty::VOID
                } else {
                    builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN)
                };
                (
                    map_fn.as_ref(),
                    vec![elem, Ty::NUMBER],
                    CallbackContract::Mapped { keep_holes: false },
                    src,
                    None,
                )
            }
            Expr::MapGroupBy { items, key_fn } => {
                let src = self.eval_expr(inst_id, receiver, items, state)?;
                if src == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let elem =
                    builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN);
                (
                    key_fn.as_ref(),
                    vec![elem, Ty::NUMBER],
                    CallbackContract::GroupKey,
                    src,
                    Some(elem),
                )
            }
            Expr::Call { callee, args, .. } => {
                let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                else {
                    return Ok(None);
                };
                let method_name = hir_compat::method(property).name();
                // Map.groupBy(items, keyFn)
                if let Expr::PropertyGet {
                    object: global,
                    property: class,
                    ..
                } = object.as_ref()
                    && matches!(global.as_ref(), Expr::GlobalGet(_))
                    && class == "Map"
                    && method_name == "groupBy"
                    && let [items, key_fn] = args.as_slice()
                {
                    let src = self.eval_expr(inst_id, receiver, items, state)?;
                    if src == Ty::BOTTOM {
                        return Ok(Some(Ty::BOTTOM));
                    }
                    let elem =
                        builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN);
                    (
                        key_fn,
                        vec![elem, Ty::NUMBER],
                        CallbackContract::GroupKey,
                        src,
                        Some(elem),
                    )
                } else {
                    let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                    if obj_ty == Ty::BOTTOM {
                        return Ok(Some(Ty::BOTTOM));
                    }
                    let (stripped, _) = self.types.strip_nullish(obj_ty);
                    // Map.forEach / Set.forEach
                    if method_name == "forEach"
                        && let TyKind::Generic { base, args: gargs } = self.types.kind(stripped)
                        && let HirType::Named(base_name) = base.hir(self.ctx)
                        && let Some(cb) = args.first()
                    {
                        let gslice: Vec<Ty> = self.types.generic_slice(gargs).to_vec();
                        match (base_name.as_str(), gslice.as_slice()) {
                            ("Map" | "ReadonlyMap", [k, v]) => (
                                cb,
                                vec![*v, *k, stripped],
                                CallbackContract::Void,
                                obj_ty,
                                None,
                            ),
                            ("Set" | "ReadonlySet", [v]) => (
                                cb,
                                vec![*v, *v, stripped],
                                CallbackContract::Void,
                                obj_ty,
                                None,
                            ),
                            _ => return Ok(None),
                        }
                    } else {
                        let Some(recv_kind) = builtins::receiver_of(self.ctx, self.types, stripped)
                        else {
                            return Ok(None);
                        };
                        let Some(method) = crate::methods::Method::lookup(recv_kind, method_name)
                        else {
                            return Ok(None);
                        };
                        let Some(cb_kind) = method.callback() else {
                            return Ok(None);
                        };
                        let Some((first_arg, tail)) = args.split_first() else {
                            return Ok(None);
                        };
                        match cb_kind {
                            crate::methods::Callback::Replacement => {
                                let Some(cb) = tail.first() else {
                                    return Ok(None);
                                };
                                let cb_ty = self.eval_expr(inst_id, receiver, cb, state)?;
                                if cb_ty == Ty::BOTTOM {
                                    return Ok(Some(Ty::BOTTOM));
                                }
                                if !matches!(self.types.kind(cb_ty), TyKind::Func(_)) {
                                    return Ok(None);
                                }
                                let mut in_tys = vec![Ty::STRING];
                                let captures = match first_arg {
                                    Expr::RegExp { pattern, .. } => {
                                        Some(crate::methods::regex_captures(pattern))
                                    }
                                    Expr::LocalGet(id) => self
                                        .index
                                        .local_idx(*id)
                                        .and_then(|li| {
                                            self.index
                                                .local_regex_pattern
                                                .get(li.0 as usize)
                                                .copied()
                                                .flatten()
                                        })
                                        .map(crate::methods::regex_captures),
                                    Expr::String(_) => Some((0, false)),
                                    _ => None,
                                };
                                if let Some((count, named)) = captures {
                                    let opt_str =
                                        self.types.with_absences(Ty::STRING, Absence::Undefined);
                                    in_tys.extend(std::iter::repeat_n(opt_str, count));
                                    in_tys.extend([Ty::NUMBER, Ty::STRING]);
                                    if named {
                                        let groups = self.types.object(&mut [], Some(opt_str));
                                        in_tys.push(groups);
                                    }
                                }
                                (cb, in_tys, CallbackContract::Replacement, obj_ty, None)
                            }
                            crate::methods::Callback::Elements => {
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let contract = match method_name {
                                    "forEach" => CallbackContract::Void,
                                    "map" => CallbackContract::Mapped {
                                        keep_holes: self.facts.sparse_arrays,
                                    },
                                    "flatMap" => CallbackContract::FlatMapped,
                                    _ => CallbackContract::Predicate,
                                };
                                (
                                    first_arg,
                                    vec![elem, Ty::NUMBER, stripped],
                                    contract,
                                    obj_ty,
                                    None,
                                )
                            }
                            crate::methods::Callback::AllIndices => {
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let opt = self.types.with_absences(elem, Absence::Undefined);
                                (
                                    first_arg,
                                    vec![opt, Ty::NUMBER, stripped],
                                    CallbackContract::Predicate,
                                    obj_ty,
                                    None,
                                )
                            }
                            crate::methods::Callback::Comparator => {
                                if matches!(first_arg, Expr::Undefined) {
                                    return Ok(None);
                                }
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let def = self.types.exclude_absences(elem, Absence::Undefined);
                                (
                                    first_arg,
                                    vec![def, def],
                                    CallbackContract::Comparator,
                                    obj_ty,
                                    None,
                                )
                            }
                            crate::methods::Callback::Fold => {
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let acc = match tail.first() {
                                    Some(init) => self.eval_expr(inst_id, receiver, init, state)?,
                                    None => elem,
                                };
                                if acc == Ty::BOTTOM {
                                    return Ok(Some(Ty::BOTTOM));
                                }
                                (
                                    first_arg,
                                    vec![acc, elem, Ty::NUMBER, stripped],
                                    CallbackContract::Fold,
                                    obj_ty,
                                    None,
                                )
                            }
                        }
                    }
                }
            }
            _ => return Ok(None),
        };

        if inputs.contains(&Ty::BOTTOM) {
            return Ok(Some(Ty::BOTTOM));
        }
        let ident = if let Expr::Closure {
            func_id, captures, ..
        } = callback
        {
            let Some(tid) = self.index.template_of_func(*func_id) else {
                return Ok(None);
            };
            let captures_local = captures.iter().any(|cap| {
                self.index
                    .local_idx(*cap)
                    .is_some_and(|l| !self.index.local_module.contains(l.0 as usize))
            });
            FuncIdentity {
                template: tid,
                capture_owner: if captures_local { Some(inst_id) } else { None },
            }
        } else {
            let cb_val = self.eval_expr(inst_id, receiver, callback, state)?;
            if cb_val == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            let TyKind::Func(ident) = self.types.kind(cb_val) else {
                return Ok(None);
            };
            ident
        };
        // Trim inputs to the callback's parameter count if it has fewer params (and no rest param).
        let Some(cb_view) = self.index.view(ident.template) else {
            return Ok(None);
        };
        let keep_inline_closure = matches!(callback, Expr::Closure { .. })
            && (cb_view.captures_this || cb_view.captures_new_target);
        if !cb_view.params.iter().any(|p| p.is_rest) && inputs.len() > cb_view.params.len() {
            inputs.truncate(cb_view.params.len());
        }
        let cb_inst = self.request_instance(InstanceReq {
            template: ident.template,
            receiver: if keep_inline_closure { receiver } else { None },
            capture_owner: ident.capture_owner,
            args: &inputs,
            explicit_type_args: &[],
            call_byte_offset: 0,
            caller: Some(inst_id),
            is_inline_callback: true,
        })?;
        self.record_site(
            inst_id,
            callback,
            SiteResolution::Callback {
                target: cb_inst,
                keep_inline_closure,
            },
        );
        let ret_loc = self
            .instances
            .get(cb_inst.0 as usize)
            .map_or(LocId(0), |i| i.return_loc);
        let cb_ret = self.read_loc(ret_loc, inst_id);

        // Compute the overall built-in expression result type!
        let result = match contract {
            CallbackContract::Void => Ty::VOID,
            CallbackContract::Mapped { keep_holes } => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else if self.types.is_open(cb_ret) {
                    Ty::OPEN
                } else {
                    let elem = if keep_holes {
                        self.types.with_absences(cb_ret, Absence::Undefined)
                    } else {
                        cb_ret
                    };
                    self.types.array(elem)
                }
            }
            CallbackContract::FlatMapped => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else if self.types.is_open(cb_ret) {
                    Ty::OPEN
                } else if matches!(self.types.kind(cb_ret), TyKind::Array(_)) {
                    cb_ret
                } else {
                    self.types.array(cb_ret)
                }
            }
            CallbackContract::Comparator => recv_ty,
            CallbackContract::Replacement => Ty::STRING,
            CallbackContract::Fold => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else if !self.types.is_open(cb_ret) && cb_ret != Ty::VOID {
                    cb_ret
                } else {
                    inputs.first().copied().unwrap_or(Ty::OPEN)
                }
            }
            CallbackContract::GroupKey => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else {
                    let key = if self.types.is_open(cb_ret) {
                        Ty::STRING
                    } else {
                        cb_ret
                    };
                    let elem = map_group_elem.unwrap_or(Ty::OPEN);
                    let arr = self.types.array(elem);
                    let base = shapes::TypeId::from_owned(self.ctx, HirType::Named("Map".into()));
                    self.types.generic(base, &[key, arr])
                }
            }
            CallbackContract::Predicate => match expr {
                Expr::ArrayFilter { .. } => recv_ty,
                Expr::ArraySome { .. } | Expr::ArrayEvery { .. } => Ty::BOOLEAN,
                Expr::ArrayFindIndex { .. } => Ty::NUMBER,
                Expr::ArrayFind { .. } => {
                    let elem = builtins::iterated_element(self.ctx, self.types, recv_ty)
                        .unwrap_or(Ty::OPEN);
                    self.types.with_absences(elem, Absence::Undefined)
                }
                Expr::Call { callee, .. } => {
                    if let Expr::PropertyGet { property, .. } = callee.as_ref() {
                        match hir_compat::method(property).name() {
                            "filter" => recv_ty,
                            "some" | "every" => Ty::BOOLEAN,
                            "findIndex" | "findLastIndex" => Ty::NUMBER,
                            "find" | "findLast" => {
                                let elem =
                                    builtins::iterated_element(self.ctx, self.types, recv_ty)
                                        .unwrap_or(Ty::OPEN);
                                self.types.with_absences(elem, Absence::Undefined)
                            }
                            _ => Ty::OPEN,
                        }
                    } else {
                        Ty::OPEN
                    }
                }
                _ => Ty::OPEN,
            },
        };
        Ok(Some(result))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_new(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        class_name: &str,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // Object literal lowered as `new __AnonShape_N(arg0, arg1, ...)`.
        if hir_compat::is_object_literal(class_name)
            && let Some(fields) = self.index.literal_fields(class_name)
        {
            let mut props = Vec::with_capacity(fields.len());
            for (field, arg) in fields.iter().zip(args) {
                self.invalidate_agg_expr(inst_id, arg);
                let ty = self.eval_expr(inst_id, receiver, arg, state)?;
                if let TyKind::Func(ident) = self.types.kind(ty) {
                    self.check_escaping_unannotated_func(ident.template)?;
                }
                let ty = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                props.push(ObjectProp {
                    name: self.types.intern_name(&field.name),
                    ty,
                    optional: false,
                    readonly: false,
                });
            }
            return Ok(self.types.object(&mut props, None));
        }

        let mut arg_tys = Vec::with_capacity(args.len());
        for arg in args {
            self.invalidate_agg_expr(inst_id, arg);
            arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
        }

        // User class (explicit generic, implicit generic, or plain class).
        if let Some(cid) = self.index.class_by_name(class_name) {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let class_args = if !type_args.is_empty() {
                let type_bindings = self.active_instance_type_bindings(inst_id);
                type_args
                    .iter()
                    .map(|ta| {
                        let t = self.types.intern_hir(self.ctx, ta, true);
                        self.types.substitute(self.ctx, t, &type_bindings)
                    })
                    .collect::<Vec<_>>()
            } else if let Some(tps) = self.class_type_params.get(cid.0 as usize)
                && !tps.is_empty()
            {
                let tps = tps.clone();
                let mut bindings = Vec::new();
                if let Some(ctor_tid) = self.index.class_ctor.get(cid.0 as usize).copied().flatten()
                    && let Some(ctor_view) = self.index.view(ctor_tid)
                {
                    for (param, &arg_ty) in ctor_view.params.iter().zip(&arg_tys) {
                        let pat = self.types.intern_hir(self.ctx, &param.ty, true);
                        self.types.unify(self.ctx, pat, arg_ty, &tps, &mut bindings);
                    }
                }
                tps.iter()
                    .map(|tp_id| {
                        bindings
                            .iter()
                            .find(|(k, _)| k == tp_id)
                            .map_or(Ty::OPEN, |(_, v)| *v)
                    })
                    .collect()
            } else if self
                .index
                .class_has_implicit_params
                .contains(cid.0 as usize)
            {
                arg_tys.clone()
            } else {
                Vec::new()
            };

            let cinst = self.request_class_inst(cid, &class_args, byte_offset, Some(inst_id))?;
            let strip_mask = callable_arg_mask(self.types, &arg_tys);
            self.record_site(
                inst_id,
                expr,
                SiteResolution::ClassNew {
                    class_inst: cinst,
                    strip_arg_mask: strip_mask,
                },
            );
            return Ok(self.types.class_inst(cinst));
        }

        if class_name == "Array" {
            return Ok(self.types.array(Ty::OPEN));
        }
        Ok(self.types.named(self.ctx, class_name))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_call(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        callee: &'h Expr,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // 1. `structuredClone(x)`
        if matches!(callee, Expr::ExternFuncRef { name, .. } if name == "structuredClone") {
            return match args.first() {
                Some(arg) => self.eval_expr(inst_id, receiver, arg, state),
                None => Ok(Ty::OPEN),
            };
        }

        // 2. Foreign (WIT) or built-in runtime module imported function call.
        if let Expr::ExternFuncRef {
            name, return_type, ..
        } = callee
        {
            let mut arg_tys = Vec::with_capacity(args.len());
            for arg in args {
                arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
            }
            // Built-in host modules (`node:timers/promises`, `node:fs/promises`).
            if let Some(res) = self.eval_builtin_import_call(name, args, &arg_tys) {
                return Ok(res);
            }
            if let Some((_, sig)) = self.decls.foreign_functions.get(name.as_str()) {
                let ret = self.types.intern_hir(self.ctx, &sig.return_type, true);
                if sig.is_async && !matches!(self.types.kind(ret), TyKind::Promise(_)) {
                    return Ok(self.types.promise(ret));
                }
                return Ok(ret);
            }
            return Ok(self.types.intern_hir(self.ctx, return_type, true));
        }

        // 3. Method call: `object.property(args...)`
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            return self.eval_method_call(
                inst_id,
                receiver,
                expr,
                object,
                property,
                args,
                byte_offset,
                state,
            );
        }

        // 4. Direct or indirect call through a static function identity (`fn(args...)`).
        let callee_ty = self.eval_expr(inst_id, receiver, callee, state)?;
        let mut arg_tys = Vec::with_capacity(args.len());
        for arg in args {
            self.invalidate_agg_expr(inst_id, arg);
            arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
        }
        if callee_ty == Ty::BOTTOM || arg_tys.contains(&Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }

        let TyKind::Func(ident) = self.types.kind(callee_ty) else {
            return Ok(Ty::OPEN);
        };
        let Some(target_view) = self.index.view(ident.template) else {
            return Ok(Ty::OPEN);
        };

        // Check `captures_this` / `captures_new_target` on callback arguments passed to user fns.
        for arg in args {
            if let Expr::Closure {
                captures_this,
                captures_new_target,
                ..
            } = arg
                && (*captures_this || *captures_new_target)
            {
                return Err(self.metadata.locations.annotate_call(
                    Diagnostic::unsupported(
                        "a callback that captures this or new.target requires its original receiver",
                    ),
                    byte_offset,
                ));
            }
        }

        // Calling a function with unannotated parameters using surplus arguments (when it has
        // no rest parameter) cannot statically close its signature.
        if !target_view.params.iter().any(|p| p.is_rest) && arg_tys.len() > target_view.params.len()
        {
            self.check_escaping_unannotated_func(ident.template)?;
        }

        // Check explicit `FunctionType` parameter annotations and escaping callback params!
        self.check_callback_params(inst_id, target_view, &mut arg_tys, byte_offset)?;

        // Bind explicit type arguments (`fn<number>(x)`), substituting active caller type params.
        let explicit_targs: Vec<Ty> = if type_args.is_empty() {
            Vec::new()
        } else {
            let caller_bindings = self.active_instance_type_bindings(inst_id);
            type_args
                .iter()
                .map(|ta| {
                    let t = self.types.intern_hir(self.ctx, ta, true);
                    self.types.substitute(self.ctx, t, &caller_bindings)
                })
                .collect()
        };

        let target_inst = self.request_instance(InstanceReq {
            template: ident.template,
            receiver: None,
            capture_owner: ident.capture_owner,
            args: &arg_tys,
            explicit_type_args: &explicit_targs,
            call_byte_offset: byte_offset,
            caller: Some(inst_id),
            is_inline_callback: false,
        })?;
        let strip_mask = callable_arg_mask(self.types, &arg_tys);
        self.record_site(
            inst_id,
            expr,
            SiteResolution::DirectCall {
                target: target_inst,
                strip_arg_mask: strip_mask,
            },
        );
        // Propagate closure-written locals so reads after `set("hi")` know `v` is initialized (S3).
        if let Some(target_written) = self.inst_written.get(target_inst.0 as usize).cloned()
            && let Some(cur_written) = self.inst_written.get_mut(inst_id.0 as usize)
        {
            cur_written.union_with(&target_written);
        }
        let ret_loc = self
            .instances
            .get(target_inst.0 as usize)
            .map_or(LocId(0), |i| i.return_loc);
        Ok(self.read_loc(ret_loc, inst_id))
    }

    pub(super) fn eval_call_spread(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        callee: &'h Expr,
        args: &'h [perry_hir::ir::CallArg],
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
            for arg in args {
                let expr = match arg {
                    perry_hir::ir::CallArg::Expr(e) | perry_hir::ir::CallArg::Spread(e) => e,
                };
                self.eval_expr(inst_id, receiver, expr, state)?;
            }
            let name = hir_compat::method(property).name();
            if matches!(name, "concat" | "filter" | "sort" | "toSorted") {
                return Ok(obj_ty);
            }
            if matches!(name, "max" | "min") {
                return Ok(Ty::NUMBER);
            }
            return Ok(Ty::OPEN);
        }

        let callee_ty = self.eval_expr(inst_id, receiver, callee, state)?;
        let mut expanded = Vec::with_capacity(args.len());
        let mut has_bottom = callee_ty == Ty::BOTTOM;
        for arg in args {
            let (expr, is_spread) = match arg {
                perry_hir::ir::CallArg::Expr(e) => (e, false),
                perry_hir::ir::CallArg::Spread(e) => (e, true),
            };
            let ty = self.eval_expr(inst_id, receiver, expr, state)?;
            if ty == Ty::BOTTOM {
                has_bottom = true;
                continue;
            }
            if is_spread {
                let (stripped, _) = self.types.strip_nullish(ty);
                match self.types.kind(stripped) {
                    TyKind::Tuple(span) => {
                        expanded.extend_from_slice(self.types.tuple_slice(span));
                    }
                    _ => {
                        let elem = builtins::iterated_element(self.ctx, self.types, stripped)
                            .unwrap_or(Ty::OPEN);
                        expanded.push(elem);
                    }
                }
            } else {
                expanded.push(ty);
            }
        }
        if has_bottom {
            return Ok(Ty::BOTTOM);
        }
        let TyKind::Func(ident) = self.types.kind(callee_ty) else {
            return Ok(Ty::OPEN);
        };
        let target_inst = self.request_instance(InstanceReq {
            template: ident.template,
            receiver: None,
            capture_owner: ident.capture_owner,
            args: &expanded,
            explicit_type_args: &[],
            call_byte_offset: 0,
            caller: Some(inst_id),
            is_inline_callback: false,
        })?;
        self.record_site(
            inst_id,
            expr,
            SiteResolution::DirectCall {
                target: target_inst,
                strip_arg_mask: 0,
            },
        );
        let ret_loc = self
            .instances
            .get(target_inst.0 as usize)
            .map_or(LocId(0), |i| i.return_loc);
        Ok(self.read_loc(ret_loc, inst_id))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_method_call(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        object: &'h Expr,
        property: &str,
        args: &'h [Expr],
        byte_offset: u32,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        let method = hir_compat::method(property);
        let name = method.name();

        // Global calls: `fetch(url, init)` -> `Promise<Response>`.
        if matches!(object, Expr::GlobalGet(_)) && name == "fetch" {
            for arg in args {
                self.eval_expr(inst_id, receiver, arg, state)?;
            }
            let resp = self.types.named(self.ctx, "Response");
            return Ok(self.types.promise(resp));
        }

        // `Writable.toWeb(stream)` imported from `node:stream` / `stream`.
        if name == "toWeb"
            && let Expr::ExternFuncRef { name: local, .. } = object
            && self.hir.imports.iter().any(|imp| {
                matches!(imp.source.as_str(), "stream" | "node:stream")
                    && imp.specifiers.iter().any(|s| {
                        matches!(
                            s,
                            perry_hir::ir::ImportSpecifier::Named {
                                imported,
                                local: loc
                            } if imported == "Writable" && loc == local
                        )
                    })
            })
        {
            for arg in args {
                self.eval_expr(inst_id, receiver, arg, state)?;
            }
            return Ok(self.types.named(self.ctx, "WritableStream"));
        }

        // Global static calls: `Promise.allSettled`, `performance.now`, etc.
        if let Expr::PropertyGet {
            object: global,
            property: class,
            ..
        } = object
            && matches!(global.as_ref(), Expr::GlobalGet(_))
        {
            let mut arg_tys = Vec::with_capacity(args.len());
            for arg in args {
                arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
            }
            let mut idx = 0usize;
            if let Some(res) = builtins::static_global_method(
                self.ctx,
                self.types,
                class,
                name,
                args,
                &mut |_, _| {
                    let t = arg_tys.get(idx).copied().unwrap_or(Ty::OPEN);
                    idx += 1;
                    t
                },
            ) {
                return Ok(res);
            }
            return Ok(Ty::OPEN);
        }

        // Temporal.* static calls.
        if let Expr::PropertyGet {
            object: ns,
            property: class,
            ..
        } = object
            && let Expr::PropertyGet {
                object: global,
                property: ns_name,
                ..
            } = ns.as_ref()
            && matches!(global.as_ref(), Expr::GlobalGet(_))
            && ns_name == "Temporal"
        {
            for arg in args {
                self.eval_expr(inst_id, receiver, arg, state)?;
            }
            return Ok(
                builtins::temporal_static_method(self.ctx, self.types, class, property)
                    .unwrap_or(Ty::OPEN),
            );
        }

        // `ClassName.staticMethod(args...)`
        if let Expr::ClassRef(class_name) = object {
            let mut arg_tys = Vec::with_capacity(args.len());
            for arg in args {
                arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
            }
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            if let Some(cid) = self.index.class_by_name(class_name)
                && let Some(tid) = self.index.class_static_method(cid, name)
            {
                let target = self.request_instance(InstanceReq {
                    template: tid,
                    receiver: None,
                    capture_owner: None,
                    args: &arg_tys,
                    explicit_type_args: &[],
                    call_byte_offset: byte_offset,
                    caller: Some(inst_id),
                    is_inline_callback: false,
                })?;
                let strip_mask = callable_arg_mask(self.types, &arg_tys);
                self.record_site(
                    inst_id,
                    expr,
                    SiteResolution::MethodCall {
                        target,
                        strip_arg_mask: strip_mask,
                    },
                );
                let ret_loc = self
                    .instances
                    .get(target.0 as usize)
                    .map_or(LocId(0), |i| i.return_loc);
                return Ok(self.read_loc(ret_loc, inst_id));
            }
            return Ok(Ty::OPEN);
        }

        let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
        let mut arg_tys = Vec::with_capacity(args.len());
        for arg in args {
            self.invalidate_agg_expr(inst_id, arg);
            arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
        }
        if obj_ty == Ty::BOTTOM {
            return Ok(Ty::BOTTOM);
        }

        // Open empty-array `.push(v1, v2, ...)`!
        if name == "push"
            && let Expr::LocalGet(id) = object
            && let Some(lidx) = self.index.local_idx(*id)
            && let Some((agg_loc, agg_kind)) = self.lookup_agg(inst_id, lidx)
        {
            match agg_kind {
                AggregateKind::Array => {
                    for &arg_ty in &arg_tys {
                        self.write_agg_value(agg_loc, arg_ty);
                    }
                }
                AggregateKind::Dictionary => {
                    self.write_loc(agg_loc, Ty::OPEN);
                }
            }
            return Ok(Ty::NUMBER);
        }

        if method == hir_compat::Method::Iteration {
            return Ok(obj_ty);
        }

        let (stripped, _) = self.types.strip_nullish(obj_ty);

        // User class method call (`new Box(2).map(...)`, `c.add(2)`, etc.)!
        let method_cinst = match self.types.kind(stripped) {
            TyKind::ClassInst(cinst_id) => Some(cinst_id),
            TyKind::Leaf(id) => {
                if let HirType::Named(class_name) = id.hir(self.ctx)
                    && let Some(cid) = self.index.class_by_name(class_name)
                {
                    Some(self.request_class_inst(cid, &[], byte_offset, Some(inst_id))?)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(cinst_id) = method_cinst
            && let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
            && let Some(method_tid) = self.index.class_method(cinst.class, name)
        {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            if let Some(method_view) = self.index.view(method_tid) {
                self.check_callback_params(inst_id, method_view, &mut arg_tys, byte_offset)?;
            }
            let target = self.request_instance(InstanceReq {
                template: method_tid,
                receiver: Some(cinst_id),
                capture_owner: None,
                args: &arg_tys,
                explicit_type_args: &[],
                call_byte_offset: byte_offset,
                caller: Some(inst_id),
                is_inline_callback: false,
            })?;
            let strip_mask = callable_arg_mask(self.types, &arg_tys);
            self.record_site(
                inst_id,
                expr,
                SiteResolution::MethodCall {
                    target,
                    strip_arg_mask: strip_mask,
                },
            );
            let ret_loc = self
                .instances
                .get(target.0 as usize)
                .map_or(LocId(0), |i| i.return_loc);
            return Ok(self.read_loc(ret_loc, inst_id));
        }

        // Built-in method call (Array/String/Bytes/Date/Map/Set/Response/Stream/Temporal/etc.).
        if let Some(rule) =
            builtins::lookup_method_result(self.ctx, self.types, stripped, name, false)
        {
            return Ok(builtins::apply_method_result(
                self.ctx,
                self.types,
                rule,
                MethodResultArgs {
                    receiver: obj_ty,
                    arg0: arg_tys.first().copied(),
                    arg0_expr: args.first(),
                    callback_result: None,
                    sparse_arrays: self.facts.sparse_arrays,
                },
            ));
        }

        Ok(Ty::OPEN)
    }

    pub(super) fn check_callback_params(
        &mut self,
        caller: InstanceId,
        target_view: &super::super::boundary::TemplateView<'h>,
        arg_tys: &mut [Ty],
        byte_offset: u32,
    ) -> Result<()> {
        let fn_display = hir_compat::display_name(target_view.name);
        for (p_idx, (param, actual)) in target_view
            .params
            .iter()
            .zip(arg_tys.iter_mut())
            .enumerate()
        {
            let _ = p_idx;
            if let HirType::Function(expected) = &param.ty {
                let TyKind::Func(ident) = self.types.kind(*actual) else {
                    if !self.types.is_open(*actual) {
                        return Err(self.metadata.locations.annotate_call(
                            diag!(
                                codes::TYPE_MISMATCH,
                                "parameter '{}' of {fn_display} requires a statically known function",
                                param.name
                            ),
                            byte_offset,
                        ));
                    }
                    continue;
                };
                if let Some(lidx) = self.index.local_idx(param.id)
                    && let Some(span) = self
                        .index
                        .local_value_use
                        .get(lidx.0 as usize)
                        .copied()
                        .flatten()
                {
                    return Err(self.metadata.locations.annotate(
                        Diagnostic::unsupported(format!(
                            "callback '{}' of {fn_display} escapes its static call site",
                            param.name
                        )),
                        span,
                    ));
                }
                let Some(cb_view) = self.index.view(ident.template) else {
                    continue;
                };
                let mismatch = || {
                    self.metadata.locations.annotate_call(
                        diag!(
                            codes::TYPE_MISMATCH,
                            "callback does not match parameter '{}' of {fn_display}",
                            param.name
                        ),
                        byte_offset,
                    )
                };
                let required = cb_view
                    .params
                    .iter()
                    .filter(|p| p.default.is_none() && !p.is_rest)
                    .count();
                if cb_view.is_async != expected.is_async
                    || cb_view.is_generator != expected.is_generator
                    || required > expected.params.len()
                {
                    return Err(mismatch());
                }
                let mut expected_param_tys = Vec::with_capacity(expected.params.len());
                for (cb_p, (_, exp_p, _)) in cb_view.params.iter().zip(&expected.params) {
                    let exp_ty = self.types.intern_hir(self.ctx, exp_p, true);
                    let cb_ty = self.types.intern_hir(self.ctx, &cb_p.ty, true);
                    if !self.types.is_open(cb_ty) && !self.types.accepts(self.ctx, cb_ty, exp_ty) {
                        return Err(mismatch());
                    }
                    expected_param_tys.push(exp_ty);
                }
                // Instantiate the callback at the declared contract parameter types and check its return.
                let bound_cb = self.request_instance(InstanceReq {
                    template: ident.template,
                    receiver: None,
                    capture_owner: ident.capture_owner,
                    args: &expected_param_tys,
                    explicit_type_args: &[],
                    call_byte_offset: byte_offset,
                    caller: Some(caller),
                    is_inline_callback: false,
                })?;
                let exp_ret = self.types.intern_hir(self.ctx, &expected.return_type, true);
                if exp_ret != Ty::VOID {
                    self.result_contracts.push(ResultContract {
                        inst: bound_cb,
                        expected: exp_ret,
                        param_name: param.name.clone(),
                        byte_offset,
                    });
                }
            } else if matches!(self.types.kind(*actual), TyKind::Func(_))
                && let Some(lidx) = self.index.local_idx(param.id)
                && let Some(span) = self
                    .index
                    .local_value_use
                    .get(lidx.0 as usize)
                    .copied()
                    .flatten()
            {
                return Err(self.metadata.locations.annotate(
                    Diagnostic::unsupported(format!(
                        "callback '{}' of {fn_display} escapes its static call site",
                        param.name
                    )),
                    span,
                ));
            }
        }
        Ok(())
    }

    pub(super) fn eval_builtin_import_call(
        &mut self,
        local_name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let mut found: Option<(&str, &str)> = None;
        for import in &self.hir.imports {
            let source = import.source.as_str();
            for spec in &import.specifiers {
                if let perry_hir::ir::ImportSpecifier::Named { imported, local } = spec
                    && local == local_name
                {
                    found = Some((source, imported.as_str()));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let (source, imported) = found?;
        match (source, imported) {
            ("timers/promises" | "node:timers/promises", "setTimeout") => {
                let payload = match arg_tys.get(1).copied() {
                    Some(t) if t == Ty::BOTTOM => return Some(Ty::BOTTOM),
                    Some(t) => t,
                    None => Ty::VOID,
                };
                Some(self.types.promise(payload))
            }
            ("fs/promises" | "node:fs/promises", "readFile") => {
                let has_encoding = args.get(1).is_some_and(|arg| {
                    matches!(arg, Expr::String(_))
                        || matches!(arg, Expr::Object(props) if props.iter().any(|(k, _)| k == "encoding"))
                });
                let inner = if has_encoding {
                    Ty::STRING
                } else {
                    self.types.named(self.ctx, "Uint8Array")
                };
                Some(self.types.promise(inner))
            }
            ("fs/promises" | "node:fs/promises", "stat") => {
                let stats = self.types.named(self.ctx, "Stats");
                Some(self.types.promise(stats))
            }
            ("fs/promises" | "node:fs/promises", "readdir") => {
                let arr = self.types.array(Ty::STRING);
                Some(self.types.promise(arr))
            }
            ("fs/promises" | "node:fs/promises", "writeFile" | "mkdir" | "unlink" | "rmdir") => {
                Some(self.types.promise(Ty::VOID))
            }
            ("perry:http-server", "request") => Some(self.types.named(self.ctx, "__PerryIncoming")),
            ("perry:http-server", "body") => {
                let bytes = self.types.named(self.ctx, "Uint8Array");
                Some(self.types.promise(bytes))
            }
            ("perry:http-server", "method" | "scheme" | "headerName") => Some(Ty::STRING),
            ("perry:http-server", "methodOther" | "schemeOther" | "authority" | "path") => {
                Some(self.types.with_absences(Ty::STRING, Absence::Undefined))
            }
            ("perry:http-server", "hasScheme") => Some(Ty::BOOLEAN),
            ("perry:http-server", "headerCount" | "respond") => Some(Ty::NUMBER),
            ("perry:http-server", "headerValue") => Some(self.types.named(self.ctx, "Uint8Array")),
            ("perry:http-server", "addHeader") => Some(Ty::VOID),
            _ => None,
        }
    }

    pub(super) fn check_escaping_unannotated_func(&self, template: TemplateId) -> Result<()> {
        let Some(view) = self.index.view(template) else {
            return Ok(());
        };
        for param in view.params {
            let annotated = self
                .hir
                .local_source_spans
                .get(&param.id)
                .is_some_and(|span| self.facts.is_param_annotated(span.start));
            if !annotated && matches!(param.ty, HirType::Any | HirType::Unknown) {
                let fn_display = hir_compat::display_name(view.name);
                let mut err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "parameter '{}' of '{fn_display}' needs a type annotation",
                    param.name
                )
                .with_help(
                    "Add a concrete type annotation to this binding, parameter, or function return.",
                );
                if let Some(span) = self.hir.local_source_spans.get(&param.id) {
                    let swc_span = swc_common::Span::new(
                        swc_common::BytePos(span.start),
                        swc_common::BytePos(span.end),
                    );
                    err = self.metadata.locations.annotate(err, swc_span);
                }
                return Err(err);
            }
        }
        Ok(())
    }

    pub(super) fn attach_instantiation_chain(
        &self,
        mut err: Diagnostic,
        mut cursor: InstanceId,
    ) -> Diagnostic {
        let mut hops = 0usize;
        while hops < 16 {
            hops += 1;
            let Some(inst) = self.instances.get(cursor.0 as usize) else {
                break;
            };
            if let Some(view) = self.index.view(inst.template)
                && inst.call_byte_offset != 0
                && !view.name.is_empty()
            {
                let display = hir_compat::display_name(view.name);
                let msg = format!("while instantiating '{display}'");
                err = match self.metadata.locations.call_span(inst.call_byte_offset) {
                    Some(note_span) => err.with_spanned_note(note_span, msg),
                    None => err.with_note(msg),
                };
            }
            let Some(next) = inst.caller else {
                break;
            };
            cursor = next;
        }
        err
    }
}
