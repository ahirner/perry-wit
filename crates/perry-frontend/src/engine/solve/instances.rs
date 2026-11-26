//! Seed reachable functions and specialize function and class instances.
use super::*;
use hir_compat::Target;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn seed_roots(&mut self) -> Result<InstanceId> {
        let init_inst = self.request_instance(InstanceReq::new(TemplateId::INIT, &[], None))?;
        self.enqueue(init_inst);
        for &owner in self.hir.exports.resources.keys() {
            self.exported_class(owner, &mut Vec::new())?;
        }
        for witness in &self.hir.exports.functions {
            let Some(target) = witness.target else {
                self.witness_instances.push(None);
                continue;
            };
            let signature = witness.signature.as_ref().map(|sig| {
                let mut resolve = |wit| self.types.export_type(self.ctx, wit);
                (
                    sig.params
                        .iter()
                        .copied()
                        .map(&mut resolve)
                        .collect::<Vec<_>>(),
                    sig.result.map(resolve),
                )
            });
            let adapter = !matches!(target, Target::Function(_))
                || signature.as_ref().is_some_and(|(params, result)| {
                    params
                        .iter()
                        .chain(result)
                        .any(|&ty| self.exported_resource(ty).is_some())
                });
            let receiver = match target {
                Target::Method(owner, _) | Target::Destructor(owner, _) => Some(Receiver::Class(
                    *self
                        .resource_instances
                        .get(&owner)
                        .ok_or_else(|| Diagnostic::internal("missing resource receiver"))?,
                )),
                _ => None,
            };
            let target = target.try_map(|fid| {
                let tid = self
                    .index
                    .template_of_func(fid)
                    .ok_or_else(|| Diagnostic::internal("missing export template"))?;
                let view = self.index.view(tid);
                let mut args = Vec::with_capacity(view.params.len());
                if let Some((params, _)) = &signature {
                    for (i, &ty) in params
                        .iter()
                        .skip(usize::from(receiver.is_some()))
                        .enumerate()
                    {
                        let ty = if let Some(class) = self
                            .exported_resource(ty)
                            .and_then(|id| self.resource_instances.get(&id))
                        {
                            self.types.class_inst(*class)
                        } else if let Some(p) = view
                            .params
                            .get(i)
                            .filter(|p| self.index.is_param_annotated(p.id))
                        {
                            self.types.intern_hir(self.ctx, &p.ty, true)
                        } else {
                            ty
                        };
                        args.push(self.instantiate_annotated_class(ty, 0, None)?);
                    }
                } else {
                    let bindings = self.default_type_param_bindings(tid);
                    for p in view.params {
                        let ty = self.types.intern_hir(
                            self.ctx,
                            &p.ty,
                            self.index.is_param_annotated(p.id),
                        );
                        let ty = self.types.substitute(ty, &bindings);
                        args.push(self.instantiate_annotated_class(ty, 0, None)?);
                    }
                }
                let mut request = InstanceReq::new(tid, &args, None).receiver(receiver);
                request.witness = (!adapter).then_some(witness);
                self.request_instance(request)
            })?;
            let root = match target {
                Target::Function(id) if !adapter => ExportRoot::Function(id),
                _ => {
                    let (params, result) = signature.ok_or_else(|| {
                        Diagnostic::internal("resource export requires a WIT signature")
                    })?;
                    let params = self.types.tuple(&params);
                    let result = match (result, target) {
                        (Some(ty), _) if self.exported_resource(ty).is_some() => self.alloc_loc(ty),
                        (
                            Some(_),
                            Target::Function(id) | Target::Method(_, id) | Target::Static(_, id),
                        ) => self.instances[id].return_loc,
                        _ => self.alloc_loc(Ty::VOID),
                    };
                    ExportRoot::Adapter {
                        target,
                        params,
                        result,
                    }
                }
            };
            self.witness_instances.push(Some(root));
        }
        Ok(init_inst)
    }

    fn exported_resource(&self, ty: Ty) -> Option<u32> {
        let TyKind::Named(name) = self.types.kind(ty) else {
            return None;
        };
        let owner = self.decls.resource(name.get(self.ctx))?;
        self.hir
            .exports
            .resources
            .contains_key(&owner)
            .then_some(owner)
    }

    fn exported_class(&mut self, owner: u32, active: &mut Vec<u32>) -> Result<ClassInstId> {
        if let Some(&id) = self.resource_instances.get(&owner) {
            return Ok(id);
        }
        let resource = self
            .hir
            .exports
            .resources
            .get(&owner)
            .ok_or_else(|| Diagnostic::internal("unknown exported resource"))?;
        let name = self.types.intern_name(&resource.class);
        let cid = self
            .index
            .class_by_name_id(name)
            .ok_or_else(|| Diagnostic::internal("unknown resource class"))?;
        let constructor = self
            .hir
            .exports
            .functions
            .iter()
            .find(|w| matches!(w.target,Some(Target::Constructor(id)) if id==owner));
        let mut args = Vec::new();
        active.push(owner);
        for &p in constructor
            .and_then(|w| w.signature.as_ref())
            .into_iter()
            .flat_map(|sig| &sig.params)
        {
            let ty = self.types.export_type(self.ctx, p);
            let ty = if let Some((id, r)) = self
                .exported_resource(ty)
                .and_then(|id| self.hir.exports.resources.get_key_value(&id))
            {
                if active.contains(id) {
                    self.types.named(self.ctx, &r.class)
                } else {
                    let class = self.exported_class(*id, active)?;
                    self.types.class_inst(class)
                }
            } else {
                ty
            };
            args.push(ty);
        }
        active.pop();
        let id = self.construct_class(cid, &args, Vec::new(), 0, None)?;
        self.resource_instances.insert(owner, id);
        Ok(id)
    }

    fn default_type_param_bindings(&mut self, tid: TemplateId) -> Vec<(shapes::TypeName, Ty)> {
        let view = self.index.view(tid);
        let tps = &self.type_param_pool[self.template_type_params[tid]];
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
            TyKind::Named(base) | TyKind::Generic { base, .. } => {
                let args = match self.types.kind(ty) {
                    TyKind::Generic { args, .. } => self.types.ty_slice(args).to_vec(),
                    _ => Vec::new(),
                };
                {
                    let nid = self.types.intern_name(base.get(self.ctx));
                    if let Some(cid) = self.index.class_by_name_id(nid) {
                        let resolved: Vec<Ty> = args
                            .iter()
                            .copied()
                            .map(|a| self.instantiate_annotated_class(a, byte_offset, caller))
                            .collect::<Result<_>>()?;
                        let cinst = self.request_class_inst(cid, &resolved, byte_offset, caller)?;
                        return Ok(self.types.class_inst(cinst));
                    }
                }
                if !args.is_empty() {
                    let resolved = args
                        .iter()
                        .map(|arg| self.instantiate_annotated_class(*arg, byte_offset, caller))
                        .collect::<Result<Vec<_>>>()?;
                    if resolved != args {
                        return Ok(self.types.generic(base, &resolved));
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
                let members = self.types.ty_slice(s).to_vec();
                let mut out = Vec::with_capacity(members.len());
                for m in members {
                    let n = self.instantiate_annotated_class(m, byte_offset, caller)?;
                    changed |= n != m;
                    out.push(n);
                }
                Ok(if changed { self.types.union(&out) } else { ty })
            }
            _ => Ok(ty),
        }
    }

    pub(super) fn request_instance(&mut self, req: InstanceReq<'_>) -> Result<InstanceId> {
        let InstanceReq {
            witness,
            template,
            receiver,
            capture_owner,
            args,
            explicit_type_args,
            call_byte_offset,
            caller,
            invocation,
        } = req;
        let view = self.index.view(template);
        let is_monomorphic_plain = matches!(view.kind, TemplateKind::Function)
            && view.type_params.is_empty()
            && !args
                .iter()
                .any(|&t| matches!(self.types.kind(t), TyKind::Func(_)))
            && view.params.iter().all(|p| {
                !matches!(p.ty, HirType::Any | HirType::Unknown | HirType::Function(_))
                    && !p.is_rest
                    && p.default.is_none()
            });
        let canonical_buf: Vec<Ty>;
        let args = if is_monomorphic_plain
            && receiver.is_none()
            && capture_owner.is_none()
            && explicit_type_args.is_empty()
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
        let is_promise_handle = matches!(
            view.kind,
            TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
        );
        let args = if is_promise_handle {
            args.get(..1).unwrap_or(&[])
        } else {
            args
        };
        let args_key = self.types.tuple(args);
        let targs_key = self.types.tuple(explicit_type_args);
        let list_result =
            if self.index.returns_empty_list.contains(template.index()) && !view.explicit_return {
                let signature = if let Some(w) = witness {
                    w.signature.as_ref()
                } else {
                    let mut candidates = self
                    .hir
                    .exports
                    .functions
                    .iter()
                    .filter(
                        |w| matches!(w.target, Some(Target::Function(id)) if id == view.func_id),
                    )
                    .filter_map(|w| w.signature.as_ref());
                    candidates.next().filter(|first| {
                        candidates.all(|s| s.result.map(|v| v.ty) == first.result.map(|v| v.ty))
                    })
                };
                caller
                    .and_then(|id| self.instances.get(id))
                    .filter(|i| i.template == template)
                    .and_then(|i| i.list_result)
                    .or_else(|| {
                        let ty = self.types.export_type(self.ctx, signature?.result?);
                        matches!(self.types.kind(ty), TyKind::Array(_)).then_some(ty)
                    })
            } else {
                None
            };
        let key: InstanceKey = (
            template,
            receiver,
            capture_owner,
            args_key,
            targs_key,
            list_result,
            invocation == Invocation::Lexical,
        );
        if let Some(&id) = self.instance_index.get(&key) {
            self.instances[id].is_witness |= witness.is_some();
            if invocation == Invocation::Inline {
                self.instances[id].invocation = invocation;
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

        let mut type_bindings = self.compute_type_param_bindings(
            template,
            receiver,
            capture_owner,
            args,
            explicit_type_args,
        );
        if witness.is_some() && type_bindings.is_empty() {
            type_bindings = self.default_type_param_bindings(template);
        }
        if caller.is_some() && !view.type_params.is_empty() {
            let own_tps = &self.type_param_pool[self.template_type_params[template]];
            for (tp, &tp_id) in view.type_params.iter().zip(own_tps) {
                if !type_bindings.iter().any(|(id, _)| *id == tp_id) {
                    let mut err = diag!(
                        codes::MISSING_TYPE_ANNOTATION,
                        "cannot infer type argument '{}' for generic function '{}'",
                        tp.name,
                        hir_compat::display_name(view.name)
                    );
                    if hir_compat::has_source_offset(call_byte_offset) {
                        err = self.hir.locations.annotate_call(err, call_byte_offset);
                    }
                    if let Some(cid) = caller {
                        err = attach_inst_chain(
                            &self.instances,
                            self.index,
                            &self.hir.locations,
                            err,
                            cid,
                        );
                    }
                    return Err(err);
                }
            }
        }

        let mut param_loc_buf = Vec::with_capacity(view.params.len());
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
            let declared = self.types.substitute(declared, &type_bindings);
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
                    && !matches!(self.types.kind(declared), TyKind::Signature { .. })
                {
                    declared
                } else {
                    incoming
                }
            };
            let loc = self.alloc_loc(bound_ty);
            param_loc_buf.push(loc);
            if let Some(local) = self.index.local_idx(param.id) {
                local_slots.push(InstanceLocalSlot { local, loc });
                if let Some(cloc) = self.capture_locs.collapsed[local] {
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
        let param_locs = self.param_locs.extend_from_slice(&param_loc_buf);

        let explicit_return = view
            .explicit_return
            .then(|| {
                let ret = self.types.intern_hir(self.ctx, view.return_type, true);
                let ret = self.types.substitute(ret, &type_bindings);
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

        let keep_original = first_instance && (witness.is_some() || is_monomorphic_plain);
        let emitted_func_id = if keep_original {
            view.func_id
        } else {
            let id = self.next_func_id;
            self.next_func_id = self.next_func_id.saturating_add(1);
            id
        };
        let emitted_name = if let Some(w) = witness.filter(|w| w.signature.is_none()) {
            w.name.clone()
        } else if template == TemplateId::INIT || keep_original {
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
            is_witness: witness.is_some(),
            list_result,
            template,
            receiver,
            capture_owner,
            args: args_key,
            explicit_type_args: targs_key,
            return_loc,
            partial_record_result: None,
            has_explicit_return: explicit_return.is_some(),
            param_locs,
            caller,
            call_byte_offset,
            invocation,
            emitted_func_id,
            emitted_name,
            emitted_method_name,
        });
        self.instance_index.insert(key, inst_id);
        self.capture_locs.slots.push(local_slots);
        self.inst_frame_locals
            .push(vec![Ty::BOTTOM; self.index.template_frame_count(template)]);
        self.inst_written.push(BitSet::default());
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
                ..SiteEntry::default()
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
    ) -> Vec<(shapes::TypeName, Ty)> {
        let mut bindings = capture_owner
            .instance()
            .map_or_else(Vec::new, |o| self.active_instance_type_bindings(o));
        if let Some(id) = receiver.and_then(Receiver::class) {
            let cinst = &self.class_insts[id];
            bindings.extend(
                self.type_param_pool[self.class_type_params[cinst.class]]
                    .iter()
                    .copied()
                    .zip(self.types.tuple_slice(cinst.args).iter().copied()),
            );
        }
        let view = self.index.view(template);
        let tp_span = self.template_type_params[template];
        if self.type_param_pool[tp_span].is_empty() {
            return bindings;
        }
        for (&tp_id, &et) in self.type_param_pool[tp_span].iter().zip(explicit_type_args) {
            if !self.types.is_open(et) {
                bindings.push((tp_id, et));
            }
        }
        self.unify_params(view.params, args, tp_span, &mut bindings);
        bindings
    }

    pub(super) fn unify_params(
        &mut self,
        params: &[perry_hir::ir::Param],
        args: &[Ty],
        tps: TypeParamSpan,
        bindings: &mut Vec<(shapes::TypeName, Ty)>,
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
                self.types
                    .unify(pat, resolved_arg, &self.type_param_pool[tps], bindings);
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
        let view = self.index.view(template);
        let name = hir_compat::function_label(view.name);
        let mut err = Diagnostic::unsupported(format!(
            "generic specialization limit of {INSTANCE_BUDGET} distinct argument lists reached for {name}"
        ));
        if let Some(caller_inst) = caller
            .map(|c| &self.instances[c])
            .filter(|i| i.template == template)
        {
            let prev_args = self.types.tuple_slice(caller_inst.args);
            for (idx, (&prev, &next)) in prev_args.iter().zip(next_args).enumerate() {
                if prev != next && self.types.contains_ty(next, prev) {
                    let param_name = view
                        .params
                        .get(idx)
                        .and_then(|p| first_type_var_name(&p.ty))
                        .or_else(|| view.type_params.get(idx).map(|tp| tp.name.as_str()))
                        .or_else(|| view.params.get(idx).map(|p| p.name.as_str()))
                        .unwrap_or("T");
                    err = err.with_note(format!("this recursive call grows type argument {param_name} by nesting its previous type"));
                    break;
                }
            }
        }
        self.hir.locations.annotate_call(err, call_byte_offset)
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
        let cinfo = self.index.class(class);
        let hir_class = cinfo.declaration;

        let bindings: Vec<_> = self.type_param_pool[self.class_type_params[class]]
            .iter()
            .copied()
            .zip(args.iter().copied())
            .collect();
        let mut field_loc_buf = Vec::with_capacity(hir_class.fields.len());
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
            let ty = self.types.substitute(ty, &bindings);
            let declared = !is_func_ty && !self.types.is_open(ty);
            let loc = self.alloc_loc(if declared { ty } else { Ty::BOTTOM });
            if declared {
                self.declared_locs.insert(loc.index());
            }
            field_loc_buf.push(loc);
        }
        let field_locs = self.field_locs.extend_from_slice(&field_loc_buf);
        let next_idx = self.class_insts.len();
        let emitted_name = if !self.class_insts.iter().any(|c| c.class == class) {
            hir_class.name.clone()
        } else {
            format!("{}__inst_{next_idx}", hir_class.name)
        };

        let cinst_id = self.class_insts.push(ClassInst {
            ty: self.types.named(self.ctx, &emitted_name),
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
