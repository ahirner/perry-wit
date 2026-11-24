//! Seed reachable functions and specialize function and class instances.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn seed_roots(&mut self) -> Result<InstanceId> {
        let init_inst = self.request_instance(InstanceReq::new(TemplateId::INIT, &[], None))?;
        self.init_inst = Some(init_inst);
        self.enqueue(init_inst);
        let roots: Vec<TemplateId> = self
            .index
            .views
            .iter_enumerated()
            .skip(1)
            .filter_map(|(tid, view)| {
                (view.is_exported || self.index.exported_obj_methods.contains(tid.index()))
                    .then_some(tid)
            })
            .collect();
        for tid in roots {
            let Some(view) = self.index.view(tid) else {
                continue;
            };
            let bindings = self.default_type_param_bindings(tid);
            let mut arg_tys = Vec::with_capacity(view.params.len());
            for param in view.params {
                let ty = self.types.intern_hir(
                    self.ctx,
                    &param.ty,
                    self.index.is_param_annotated(param.id),
                );
                let ty = self.types.substitute(self.ctx, ty, &bindings);
                arg_tys.push(self.instantiate_annotated_class(ty, 0, None)?);
            }
            let inst = self.request_instance(InstanceReq::new(tid, &arg_tys, None))?;
            self.enqueue(inst);
        }
        Ok(init_inst)
    }

    fn default_type_param_bindings(&mut self, tid: TemplateId) -> Vec<(shapes::TypeId, Ty)> {
        let Some(view) = self.index.view(tid) else {
            return Vec::new();
        };
        let Some(tps) = self.template_type_params.get(tid.index()) else {
            return Vec::new();
        };
        view.type_params
            .iter()
            .zip(tps)
            .filter_map(|(tp, &tp_id)| {
                Some((
                    tp_id,
                    self.types.intern_hir(
                        self.ctx,
                        tp.constraint.as_deref().or(tp.default.as_deref())?,
                        true,
                    ),
                ))
            })
            .collect()
    }

    pub(super) fn instantiate_annotated_class(
        &mut self,
        ty: Ty,
        byte_offset: u32,
        caller: Option<InstanceId>,
    ) -> Result<Ty> {
        match self.types.kind(ty) {
            TyKind::Leaf(base) | TyKind::Generic { base, .. } => {
                let args = match self.types.kind(ty) {
                    TyKind::Generic { args, .. } => self.types.ty_slice(args).to_vec(),
                    _ => Vec::new(),
                };
                if let HirType::Named(name) = base.hir(self.ctx) {
                    let nid = self.types.intern_name(name);
                    if let Some(cid) = self.index.class_by_name_id(nid) {
                        let resolved: Vec<Ty> = args
                            .into_iter()
                            .map(|a| self.instantiate_annotated_class(a, byte_offset, caller))
                            .collect::<Result<_>>()?;
                        let cinst = self.request_class_inst(cid, &resolved, byte_offset, caller)?;
                        return Ok(self.types.class_inst(cinst));
                    }
                }
                Ok(ty)
            }
            TyKind::Array(i) => {
                let n = self.instantiate_annotated_class(i, byte_offset, caller)?;
                Ok(if n == i { ty } else { self.types.array(n) })
            }
            TyKind::Promise(i) => {
                let n = self.instantiate_annotated_class(i, byte_offset, caller)?;
                Ok(if n == i { ty } else { self.types.promise(n) })
            }
            TyKind::Union(s) => {
                let mut changed = false;
                let mut out = Vec::with_capacity(s.len as usize);
                for m in self.types.ty_slice(s).to_vec() {
                    let n = self.instantiate_annotated_class(m, byte_offset, caller)?;
                    changed |= n != m;
                    out.push(n);
                }
                Ok(if changed { self.types.union(&out) } else { ty })
            }
            _ => Ok(ty),
        }
    }

    fn collect_non_global_caps(&self, captures: &[LocalId], out: &mut Vec<LocalId>) -> bool {
        let mut any = false;
        for &cap in captures {
            if self.index.is_non_global_local(cap) {
                any = true;
                if !out.contains(&cap) {
                    out.push(cap);
                }
            }
        }
        any
    }

    pub(super) fn request_instance(&mut self, req: InstanceReq<'_>) -> Result<InstanceId> {
        let InstanceReq {
            template,
            receiver,
            capture_owner,
            args,
            explicit_type_args,
            call_byte_offset,
            caller,
            is_inline_callback,
        } = req;
        let is_monomorphic_plain = self.index.view(template).is_some_and(|v| {
            matches!(v.kind, TemplateKind::Function)
                && v.type_params.is_empty()
                && !args
                    .iter()
                    .any(|&t| matches!(self.types.kind(t), TyKind::Func(_)))
                && v.params.iter().all(|p| {
                    !matches!(p.ty, HirType::Any | HirType::Unknown | HirType::Function(_))
                        && !p.is_rest
                        && p.default.is_none()
                })
        });
        let canonical_buf: Vec<Ty>;
        let args = if is_monomorphic_plain
            && receiver.is_none()
            && capture_owner.is_none()
            && explicit_type_args.is_empty()
            && let Some(view) = self.index.view(template)
            && !view.params.is_empty()
        {
            let mut declared = Vec::with_capacity(view.params.len());
            for param in view.params {
                let ty = self.types.intern_hir(self.ctx, &param.ty, true);
                declared.push(self.instantiate_annotated_class(ty, 0, None)?);
            }
            canonical_buf = declared;
            canonical_buf.as_slice()
        } else {
            args
        };
        let is_promise_handle = self.index.view(template).is_some_and(|v| {
            matches!(
                v.kind,
                TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
            )
        });
        let args = if is_promise_handle {
            args.get(..1).unwrap_or(&[])
        } else {
            args
        };
        let args_key = self.types.tuple(args);
        let targs_key = self.types.tuple(explicit_type_args);
        let key: InstanceKey = (template, receiver, capture_owner, args_key, targs_key);
        if let Some(&id) = self.instance_index.get(&key) {
            if is_inline_callback && let Some(inst) = self.instances.get_mut(id) {
                inst.is_inline_callback = true;
            }
            if self.inst_superseded.remove(id.index()) {
                bump_count(&mut self.template_counts, template);
                self.enqueue(id);
            }
            return Ok(id);
        }

        if self.template_counts[template] >= INSTANCE_BUDGET {
            self.prune_unreachable();
            if self.template_counts[template] >= INSTANCE_BUDGET {
                return Err(self.budget_error(template, args, caller, call_byte_offset));
            }
        }
        let first_instance = self.template_counts[template] == 0;
        bump_count(&mut self.template_counts, template);

        let view = self
            .index
            .view(template)
            .ok_or_else(|| Diagnostic::internal("inference engine requested unknown template"))?;

        let mut inst_caps = Vec::new();
        let mut captures_non_global = capture_owner.is_some();
        if capture_owner.is_some() {
            captures_non_global |= self.collect_non_global_caps(view.captures, &mut inst_caps);
        }
        for &arg in args {
            if let TyKind::Func(ident) = self.types.kind(arg)
                && let Some(cb) = self.index.view(ident.template)
            {
                captures_non_global |= self.collect_non_global_caps(cb.captures, &mut inst_caps);
            }
        }

        let mut type_bindings = self.compute_type_param_bindings(
            template,
            receiver,
            capture_owner,
            args,
            explicit_type_args,
        );
        if view.is_exported && type_bindings.is_empty() {
            type_bindings = self.default_type_param_bindings(template);
        }
        if caller.is_some()
            && !view.type_params.is_empty()
            && let Some(own_tps) = self.template_type_params.get(template.index())
        {
            for (tp, &tp_id) in view.type_params.iter().zip(own_tps) {
                if !type_bindings.iter().any(|(id, _)| *id == tp_id) {
                    let mut err = diag!(
                        codes::MISSING_TYPE_ANNOTATION,
                        "cannot infer type argument '{}' for generic function '{}'",
                        tp.name,
                        hir_compat::display_name(view.name)
                    );
                    if hir_compat::has_source_offset(call_byte_offset) {
                        err = self.metadata.locations.annotate_call(err, call_byte_offset);
                    }
                    if let Some(cid) = caller {
                        err = attach_inst_chain(
                            &self.instances,
                            self.index,
                            &self.metadata.locations,
                            err,
                            cid,
                        );
                    }
                    return Err(err);
                }
            }
        }

        let param_loc_start = self.param_locs.len() as u32;
        let mut local_slots = Vec::with_capacity(view.params.len() + 4);
        let fixed_count = view
            .params
            .iter()
            .position(|p| p.is_rest)
            .unwrap_or(view.params.len());
        for (p_idx, param) in view.params.iter().enumerate() {
            let declared =
                self.types
                    .intern_hir(self.ctx, &param.ty, self.index.is_param_annotated(param.id));
            let declared = self.types.substitute(self.ctx, declared, &type_bindings);
            let bound_ty = if param.is_rest {
                let extra_args = args.get(fixed_count..).unwrap_or(&[]);
                if !extra_args.is_empty() {
                    let elem = extra_args.iter().fold(Ty::BOTTOM, |acc, &x| {
                        let norm = self.types.normalize_for_storage(self.ctx, self.decls, x);
                        self.types.join(acc, norm)
                    });
                    self.types.array(elem)
                } else if !self.types.is_open(declared) {
                    declared
                } else {
                    self.types.array(Ty::NEVER)
                }
            } else {
                let incoming = args.get(p_idx).copied().unwrap_or(Ty::VOID);
                if incoming == Ty::OPEN || incoming == Ty::BOTTOM {
                    declared
                } else if matches!(self.types.kind(incoming), TyKind::Func(_)) {
                    incoming
                } else if param.default.is_some() {
                    if !self.types.is_open(declared) {
                        declared
                    } else {
                        let s = self.types.exclude_absences(incoming, Absence::Undefined);
                        if s == Ty::NEVER { Ty::BOTTOM } else { s }
                    }
                } else if !self.types.is_open(declared)
                    && !matches!(self.types.kind(declared), TyKind::Leaf(id) if matches!(id.hir(self.ctx), HirType::Function(_)))
                {
                    declared
                } else {
                    incoming
                }
            };
            let loc = self.alloc_loc(bound_ty);
            self.param_locs.push(loc);
            if let Some(local) = self.index.local_idx(param.id) {
                local_slots.push(InstanceLocalSlot { local, loc });
                if let Some(cloc) = self
                    .collapsed_local_locs
                    .get(local.index())
                    .copied()
                    .flatten()
                {
                    self.write_loc(cloc, bound_ty);
                }
            }
        }
        for &cap_local in self.index.template_captured_locals(template) {
            if !local_slots.iter().any(|s| s.local == cap_local) {
                let loc = self.alloc_loc(Ty::BOTTOM);
                local_slots.push(InstanceLocalSlot {
                    local: cap_local,
                    loc,
                });
            }
        }
        let param_locs = TySpan {
            start: param_loc_start,
            len: view.params.len() as u32,
        };

        let explicit_return = view
            .explicit_return
            .then(|| {
                let ret = self.types.intern_hir(self.ctx, view.return_type, true);
                let ret = self.types.substitute(self.ctx, ret, &type_bindings);
                (!self.types.is_open(ret)).then_some(ret)
            })
            .flatten();
        let return_loc = if view.is_async && explicit_return.is_none() {
            let inner_loc = self.alloc_loc(Ty::BOTTOM);
            let cell = self.types.cell(inner_loc, CellKind::Promise);
            self.alloc_loc(cell)
        } else {
            self.alloc_loc(explicit_return.unwrap_or(Ty::BOTTOM))
        };

        let keep_original = first_instance
            && (view.is_exported
                || self.index.exported_obj_methods.contains(template.index())
                || is_monomorphic_plain);
        let emitted_func_id = if keep_original {
            view.func_id
        } else {
            let id = self.next_func_id;
            self.next_func_id = self.next_func_id.saturating_add(1);
            id
        };
        let emitted_name = if template == TemplateId::INIT || keep_original {
            view.raw_name()
        } else {
            format!("{}_instance_{emitted_func_id}", view.raw_name())
        };
        let emitted_method_name = match view.kind {
            TemplateKind::ClassMethod
            | TemplateKind::ClassStaticMethod(_)
            | TemplateKind::ClassGetter(_)
            | TemplateKind::ClassSetter(_) => {
                if self
                    .instances
                    .iter()
                    .any(|i| i.template == template && i.receiver == receiver)
                {
                    format!("{}__inst_{emitted_func_id}", view.name)
                } else {
                    view.name.to_owned()
                }
            }
            _ => String::new(),
        };

        let inst_id = self.instances.push(Instance {
            template,
            receiver,
            capture_owner,
            args: args_key,
            explicit_type_args: targs_key,
            return_loc,
            has_explicit_return: explicit_return.is_some(),
            param_locs,
            caller,
            call_byte_offset,
            is_inline_callback,
            captures_non_global,
            emitted_func_id,
            emitted_name,
            emitted_method_name,
        });
        self.instance_index.insert(key, inst_id);
        self.inst_locals.push(local_slots);
        self.inst_frame_locals
            .push(vec![Ty::BOTTOM; self.index.template_frame_count(template)]);
        self.inst_written.push(BitSet::default());
        self.inst_captures.push(inst_caps);
        let sites = self
            .index
            .sites(template)
            .iter()
            .map(|site| SiteEntry {
                resolution: site.handles.map(|(resolve_tid, reject_tid)| {
                    SiteResolution::PromiseNew {
                        value_loc: self.alloc_loc(Ty::BOTTOM),
                        resolve_tid,
                        reject_tid,
                        executor: None,
                    }
                }),
                coercion: None,
            })
            .collect();
        self.inst_sites.push(sites);
        self.inst_reads.push(Vec::new());
        self.enqueue(inst_id);
        Ok(inst_id)
    }

    pub(super) fn compute_type_param_bindings(
        &mut self,
        template: TemplateId,
        receiver: Option<Receiver>,
        capture_owner: CaptureOwner,
        args: &[Ty],
        explicit_type_args: &[Ty],
    ) -> Vec<(shapes::TypeId, Ty)> {
        let mut bindings = capture_owner
            .instance()
            .map_or_else(Vec::new, |o| self.active_instance_type_bindings(o));
        if let Some(cinst) = receiver
            .and_then(Receiver::class)
            .and_then(|id| self.class_insts.get(id))
            && let Some(class_tps) = self.class_type_params.get(cinst.class.index())
        {
            bindings.extend(
                class_tps
                    .iter()
                    .copied()
                    .zip(self.types.tuple_slice(cinst.args).iter().copied()),
            );
        }
        let Some(view) = self.index.view(template) else {
            return bindings;
        };
        let Some(tps) = self
            .template_type_params
            .get(template.index())
            .filter(|t| !t.is_empty())
            .cloned()
        else {
            return bindings;
        };
        for (&tp_id, &et) in tps.iter().zip(explicit_type_args) {
            if !self.types.is_open(et) {
                bindings.push((tp_id, et));
            }
        }
        self.unify_params(view.params, args, &tps, &mut bindings);
        bindings
    }

    pub(super) fn unify_params(
        &mut self,
        params: &[perry_hir::ir::Param],
        args: &[Ty],
        tps: &[shapes::TypeId],
        bindings: &mut Vec<(shapes::TypeId, Ty)>,
    ) {
        for skip_open in [true, false] {
            for (param, &arg_ty) in params.iter().zip(args) {
                if skip_open
                    && self
                        .types
                        .is_open_with(arg_ty, self.locations.as_slice(), &self.dict_reads)
                {
                    continue;
                }
                let resolved_arg = match self.types.kind(arg_ty) {
                    TyKind::Cell {
                        loc,
                        kind: CellKind::Promise,
                    } => {
                        let inner = self.locations[loc];
                        if inner == Ty::BOTTOM {
                            self.types.promise(Ty::VOID)
                        } else {
                            self.types.promise(inner)
                        }
                    }
                    _ => arg_ty,
                };
                let pat = self.types.intern_hir(self.ctx, &param.ty, true);
                self.types.unify(self.ctx, pat, resolved_arg, tps, bindings);
            }
        }
    }

    fn budget_error(
        &self,
        template: TemplateId,
        next_args: &[Ty],
        caller: Option<InstanceId>,
        call_byte_offset: u32,
    ) -> Diagnostic {
        let name = self
            .index
            .view(template)
            .map_or("function", |v| hir_compat::function_label(v.name));
        let mut err = Diagnostic::unsupported(format!(
            "generic specialization limit of {INSTANCE_BUDGET} distinct argument lists reached for {name}"
        ));
        if let Some(caller_inst) = caller
            .and_then(|c| self.instances.get(c))
            .filter(|i| i.template == template)
        {
            let prev_args = self.types.tuple_slice(caller_inst.args);
            let view = self.index.view(template);
            for (idx, (&prev, &next)) in prev_args.iter().zip(next_args).enumerate() {
                if prev != next && self.types.contains_ty(next, prev) {
                    let param_name = view
                        .and_then(|v| {
                            v.params
                                .get(idx)
                                .and_then(|p| first_type_var_name(&p.ty))
                                .or_else(|| v.type_params.get(idx).map(|tp| tp.name.as_str()))
                                .or_else(|| v.params.get(idx).map(|p| p.name.as_str()))
                        })
                        .unwrap_or("T");
                    err = err.with_note(format!("this recursive call grows type argument {param_name} by nesting its previous type"));
                    break;
                }
            }
        }
        self.metadata.locations.annotate_call(err, call_byte_offset)
    }

    pub(super) fn request_class_inst(
        &mut self,
        class: ClassId,
        args: &[Ty],
        byte_offset: u32,
        caller: Option<InstanceId>,
    ) -> Result<ClassInstId> {
        let args_key = self.types.tuple(args);
        let key = (class, args_key);
        if let Some(&id) = self.class_index.get(&key) {
            return Ok(id);
        }
        let cinfo = self
            .index
            .class(class)
            .ok_or_else(|| Diagnostic::internal("unknown class index"))?;
        let hir_class = self
            .hir
            .classes
            .get(cinfo.hir_idx)
            .ok_or_else(|| Diagnostic::internal("unknown class in HIR"))?;

        let bindings: Vec<_> = self
            .class_type_params
            .get(class.index())
            .into_iter()
            .flatten()
            .copied()
            .zip(args.iter().copied())
            .collect();
        let field_loc_start = self.field_locs.len() as u32;
        for field in &hir_class.fields {
            let is_func_ty = matches!(field.ty, HirType::Function(_));
            let ty = self.types.intern_hir(
                self.ctx,
                &field.ty,
                !matches!(
                    field.ty,
                    HirType::Any | HirType::Unknown | HirType::Function(_)
                ),
            );
            let ty = self.types.substitute(self.ctx, ty, &bindings);
            let declared = !is_func_ty && !self.types.is_open(ty);
            let loc = self.alloc_loc(if declared { ty } else { Ty::BOTTOM });
            if declared {
                self.declared_locs.insert(loc.index());
            }
            self.field_locs.push(loc);
        }
        let field_locs = TySpan {
            start: field_loc_start,
            len: hir_class.fields.len() as u32,
        };
        let next_idx = self.class_insts.len();
        let emitted_name = if !self.class_insts.iter().any(|c| c.class == class) {
            hir_class.name.clone()
        } else {
            format!("{}__inst_{next_idx}", hir_class.name)
        };

        let cinst_id = self.class_insts.push(ClassInst {
            class,
            args: args_key,
            field_locs,
            field_init: None,
            ctor_inst: None,
            emitted_name,
        });
        self.class_index.insert(key, cinst_id);

        if let Some(template) = cinfo.field_init {
            let init = self.request_instance(
                InstanceReq::new(template, &[], caller)
                    .receiver(Some(Receiver::Class(cinst_id)))
                    .at(byte_offset),
            )?;
            self.class_insts[cinst_id].field_init = Some(init);
        }
        if let Some(ctor_tid) = cinfo.ctor {
            let ctor_inst = self.request_instance(
                InstanceReq::new(ctor_tid, args, caller)
                    .receiver(Some(Receiver::Class(cinst_id)))
                    .at(byte_offset),
            )?;
            if let Some(ci) = self.class_insts.get_mut(cinst_id) {
                ci.ctor_inst = Some(ctor_inst);
            }
        }
        Ok(cinst_id)
    }
}
