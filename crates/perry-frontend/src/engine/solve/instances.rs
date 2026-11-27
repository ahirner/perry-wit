//! Seed reachable functions and specialize function and class instances.
use super::*;
use hir_compat::Target;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn seed_roots<'r: 'h>(
        &mut self,
        witnesses: Vec<hir_compat::SourceWitness<'r>>,
        mut pending: hir_compat::ResourceClasses<ClassId>,
    ) -> Result<(InstanceId, hir_compat::Witnesses<'r, ExportRoot>)> {
        let init_inst = self.request_instance(InstanceReq::new(TemplateId::INIT, &[], None))?;
        self.enqueue(init_inst);
        let mut constructors = HashMap::new();
        for w in &witnesses {
            if let Some(sig) = &w.signature {
                match w.target {
                    Some(Target::Constructor(owner)) => {
                        constructors.entry(owner).or_insert(sig.params.as_slice());
                    }
                    Some(Target::Function(id)) => {
                        let tid = self
                            .index
                            .template_of_func(id)
                            .ok_or_else(|| Diagnostic::internal("missing export template"))?;
                        let previous = self.list_exports[tid].get_or_insert(sig.result);
                        if previous.map(|v| v.ty) != sig.result.map(|v| v.ty) {
                            *previous = None;
                        }
                    }
                    _ => {}
                }
            }
        }
        while let Some((owner, resource)) = pending.pop_first() {
            self.exported_class(
                owner,
                resource,
                &mut pending,
                &constructors,
                &mut Vec::new(),
            )?;
        }
        let mut exports = Vec::with_capacity(witnesses.len());
        for witness in witnesses {
            let Some(target) = witness.target else {
                exports.push(witness.with_target(None));
                continue;
            };
            let signature = witness.signature.as_ref();
            let adapter = !matches!(target, Target::Function(_))
                || signature.is_some_and(|sig| {
                    sig.params
                        .iter()
                        .chain(&sig.result)
                        .any(|&value| self.exported_instance(value).is_some())
                });
            let receiver = match target {
                Target::Method(owner, _) | Target::Destructor(owner, _) => Some(Receiver::Class(
                    self.resources
                        .get(&owner)
                        .map(|r| r.class)
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
                let args = if let Some(sig) = signature {
                    let mut args = Vec::with_capacity(sig.params.len());
                    for (i, &value) in sig
                        .params
                        .iter()
                        .skip(usize::from(receiver.is_some()))
                        .enumerate()
                    {
                        let ty = if let Some(class) = self.exported_instance(value) {
                            self.types.class_inst(class)
                        } else if let Some(p) = view.params.get(i).filter(|p| {
                            self.index.local_idx(p.id).is_some_and(|id| {
                                self.index.locals.rows[id]
                                    .flags
                                    .contains(LocalFlags::PARAM_ANNOTATED)
                            })
                        }) {
                            self.index.locals.rows[self.index.param_local(p)?].declared
                        } else {
                            self.types.export_type(self.ctx, value)
                        };
                        args.push(self.instantiate_annotated_class(ty, 0, None)?);
                    }
                    args
                } else {
                    self.declared_arguments(tid)?
                };
                let mut request = InstanceReq::new(tid, &args, None).receiver(receiver);
                request.witness = (!adapter).then_some(&witness);
                self.request_instance(request)
            })?;
            let root = match target {
                Target::Function(id) if !adapter => ExportRoot::Function(id),
                _ => {
                    let signature = signature.ok_or_else(|| {
                        Diagnostic::internal("resource export requires a WIT signature")
                    })?;
                    let params: Vec<_> = signature
                        .params
                        .iter()
                        .map(|&value| self.types.export_type(self.ctx, value))
                        .collect();
                    let params = self.types.tuple(&params);
                    let result = match (signature.result, target) {
                        (Some(value), _) if self.exported_instance(value).is_some() => {
                            AdapterResult::Declared(self.types.export_type(self.ctx, value))
                        }
                        (
                            Some(_),
                            Target::Function(id) | Target::Method(_, id) | Target::Static(_, id),
                        ) => AdapterResult::Inferred(self.instances[id].return_loc),
                        _ => AdapterResult::Declared(Ty::VOID),
                    };
                    ExportRoot::Adapter {
                        target,
                        params,
                        result,
                    }
                }
            };
            exports.push(witness.with_target(Some(root)));
        }
        Ok((init_inst, exports))
    }

    pub(super) fn declared_arguments(&mut self, template: TemplateId) -> Result<Vec<Ty>> {
        let bindings = self.default_type_param_bindings(template);
        self.index
            .view(template)
            .params
            .iter()
            .map(|p| {
                let declared = self.index.locals.rows[self.index.param_local(p)?].declared;
                let ty = self.types.substitute(declared, &bindings);
                self.instantiate_annotated_class(ty, 0, None)
            })
            .collect()
    }

    fn exported_instance(&self, value: crate::wit::WitValue<'_>) -> Option<ClassInstId> {
        Some(self.resources.get(&value.resource()?)?.class)
    }

    fn exported_class(
        &mut self,
        owner: crate::wit::ResourceId,
        resource: hir_compat::ResourceClass<ClassId>,
        pending: &mut hir_compat::ResourceClasses<ClassId>,
        constructors: &HashMap<crate::wit::ResourceId, &[crate::wit::WitValue<'_>]>,
        active: &mut Vec<(crate::wit::ResourceId, ClassId)>,
    ) -> Result<ClassInstId> {
        active.push((owner, resource.class));
        let mut args = Vec::new();
        for &param in constructors.get(&owner).copied().unwrap_or_default() {
            let ty = match param.resource() {
                Some(id) if let Some(&(_, class)) = active.iter().find(|(key, _)| *key == id) => {
                    self.index.class(class).ty
                }
                Some(id) if let Some(declaration) = pending.remove(&id) => {
                    let instance =
                        self.exported_class(id, declaration, pending, constructors, active)?;
                    self.types.class_inst(instance)
                }
                Some(id) if let Some(done) = self.resources.get(&id) => {
                    self.types.class_inst(done.class)
                }
                _ => self.types.export_type(self.ctx, param),
            };
            args.push(ty);
        }
        active.pop();
        let instance = self.construct_class(resource.class, &args, Vec::new(), 0, None)?;
        self.resources.insert(owner, resource.with_class(instance));
        Ok(instance)
    }

    fn default_type_param_bindings(&mut self, tid: TemplateId) -> Vec<(shapes::TypeName, Ty)> {
        let view = self.index.view(tid);
        view.type_params
            .iter()
            .filter_map(|tp| {
                Some((
                    shapes::TypeName::new(self.ctx, tp.name.clone()),
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
        let kind = self.types.kind(ty);
        let resolved = match kind {
            TyKind::Array(_) | TyKind::Promise(_) | TyKind::Union(_) | TyKind::Generic { .. } => {
                self.try_map_children(ty, |s, child| {
                    s.instantiate_annotated_class(child, byte_offset, caller)
                })?
            }
            _ => ty,
        };
        let Some((base, args)) = self.types.kind(resolved).nominal() else {
            return Ok(resolved);
        };
        let Some(class) = self.decls.symbol(base).and_then(|s| s.class_id()) else {
            return Ok(resolved);
        };
        let args = args.map_or_else(Vec::new, |s| self.types.ty_slice(s).to_vec());
        let instance = self.request_class_inst(class, &args, byte_offset, caller)?;
        Ok(self.types.class_inst(instance))
    }

    pub(super) fn regex_annotation(&self, ty: Ty) -> bool {
        matches!(
            builtins::receiver_of(self.ctx, self.decls, self.types, ty),
            Some(crate::methods::Receiver::Value(crate::methods::Kind::Regex))
        )
    }

    fn specialized_argument(&self, ty: Ty, declared: Option<Ty>) -> bool {
        match self.types.kind(ty) {
            TyKind::Func(_) => true,
            TyKind::Regex(_) => {
                declared.is_none_or(|d| self.types.is_open(d) || self.regex_annotation(d))
            }
            _ => self.arrays.contains(self.types, ty),
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
        let receiver = if view.captures_this {
            capture_owner.receiver(&self.instances)
        } else {
            receiver
        };
        let is_monomorphic_plain = matches!(view.kind, TemplateKind::Function)
            && view.type_params.is_empty()
            && !args.iter().any(|&ty| self.specialized_argument(ty, None))
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
            canonical_buf = self.declared_arguments(template)?;
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
        let inputs = self.types.tuple(args);
        let args_key = self.arrays.argument_key(self.types, inputs);
        let list_result =
            if self.index.returns_empty_list.contains(template.index()) && !view.explicit_return {
                let result = match witness {
                    Some(w) => w.signature.as_ref().and_then(|s| s.result),
                    None => self.list_exports[template].flatten(),
                };
                caller
                    .and_then(|id| self.instances.get(id))
                    .filter(|i| i.template == template)
                    .and_then(|i| i.list_result)
                    .or_else(|| {
                        let ty = self.types.export_type(self.ctx, result?);
                        matches!(self.types.kind(ty), TyKind::Array(_)).then_some(ty)
                    })
            } else {
                None
            };
        let receiver_key = receiver.map(|receiver| match receiver {
            Receiver::Object(ty) => Receiver::Object(self.arrays.argument_key(self.types, ty)),
            Receiver::Class(class) => Receiver::Class(class),
        });
        let declaration = GenericOwner::Function(template);
        let mut arguments = vec![Ty::OPEN; view.type_params.len()];
        for (slot, &ty) in arguments.iter_mut().zip(explicit_type_args) {
            if !self.types.is_open(ty) {
                *slot = ty;
            }
        }
        if arguments.contains(&Ty::OPEN) {
            self.unify_params(view.params, args, declaration, &mut arguments, caller)?;
        }
        if witness.is_some() {
            for (slot, tp) in arguments.iter_mut().zip(view.type_params) {
                if *slot == Ty::OPEN
                    && let Some(default) = tp.constraint.as_deref().or(tp.default.as_deref())
                {
                    *slot = self.types.intern_hir(self.ctx, default, true);
                }
            }
        }
        let type_arguments = self.types.tuple(&arguments);
        let key: InstanceKey = (
            template,
            receiver_key,
            capture_owner,
            args_key,
            type_arguments,
            list_result,
            invocation == Invocation::Lexical,
        );
        if let Some(&id) = self.instance_index.get(&key) {
            self.share_array_layouts(inputs, self.instances[id].args);
            if let (Some(Receiver::Object(actual)), Some(Receiver::Object(original))) =
                (receiver, self.instances[id].receiver)
            {
                self.share_array_layouts(actual, original);
            }
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

        if let Some(caller) = caller
            && let Some((param, _)) = view
                .type_params
                .iter()
                .zip(&arguments)
                .find(|(_, ty)| **ty == Ty::OPEN)
        {
            let err = diag!(
                codes::MISSING_TYPE_ANNOTATION,
                "cannot infer type argument '{}' for generic function '{}'",
                param.name,
                hir_compat::display_name(&view.name)
            );
            let err = self.hir.locations.annotate_call(err, call_byte_offset);
            return Err(attach_inst_chain(
                &self.instances,
                self.index,
                &self.hir.locations,
                err,
                caller,
            ));
        }
        let mut type_bindings = Vec::new();
        if !arguments.is_empty() {
            type_bindings.push(TypeScope {
                declaration,
                arguments: type_arguments,
            });
        }
        if let Some(class) = receiver.and_then(Receiver::class) {
            let instance = &self.class_insts[class];
            let declaration = GenericOwner::Class(instance.class);
            if !self.types.tuple_slice(instance.args).is_empty() {
                type_bindings.push(TypeScope {
                    declaration,
                    arguments: instance.args,
                });
            }
        }
        if let Some(owner) = capture_owner.instance() {
            type_bindings.extend_from_slice(&self.type_bindings[owner]);
        }
        let activation_slots = self.index.activation_slots(template);
        let mut local_slots = Vec::with_capacity(activation_slots);
        let fixed_count = view
            .params
            .iter()
            .position(|p| p.is_rest)
            .unwrap_or(view.params.len());
        for (p_idx, param) in view.params.iter().enumerate() {
            let local = self.index.param_local(param)?;
            let declared = self.index.locals.rows[local].declared;
            let declared = self.index.substitute(self.types, declared, &type_bindings);
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
                } else if self.specialized_argument(incoming, Some(declared)) {
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
            local_slots.push(bound_ty);
            if let Some(cloc) = self.collapsed_locals[local] {
                self.write_loc(cloc, bound_ty);
            }
        }
        local_slots.resize(activation_slots, Ty::BOTTOM);

        let explicit_return = view
            .explicit_return
            .then(|| {
                let ret = view.result;
                let ret = self.index.substitute(self.types, ret, &type_bindings);
                (!self.types.is_open(ret)).then_some(ret)
            })
            .flatten();
        let return_loc = if view.is_async && explicit_return.is_none() {
            let inner_loc = self.facts.alloc(Ty::BOTTOM);
            let cell = self.types.cell(inner_loc, CellKind::Promise);
            self.facts.alloc(cell)
        } else {
            self.facts.alloc(explicit_return.unwrap_or(Ty::BOTTOM))
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
        } else if view.kind.is_class_member()
            || matches!(
                view.kind,
                TemplateKind::ClassConstructor | TemplateKind::FieldInitializers
            )
        {
            format!("{}{emitted_func_id}", self.hir.member_prefix)
        } else if template == TemplateId::INIT || keep_original {
            view.raw_name()
        } else {
            format!("{}_instance_{emitted_func_id}", view.raw_name())
        };
        let inst_id = self.instances.push(Instance {
            activation: self.facts.activation(local_slots),
            is_witness: witness.is_some(),
            list_result,
            template,
            receiver,
            capture_owner,
            args: inputs,
            return_loc,
            partial_record_result: None,
            has_explicit_return: explicit_return.is_some(),
            caller,
            call_byte_offset,
            invocation,
            emitted_func_id,
            emitted_name,
        });
        self.type_bindings.push(type_bindings.into_boxed_slice());
        self.instance_index.insert(key, inst_id);
        self.inst_written.push(BitSet::default());
        let mut sites = InstanceSites::default();
        for &(site, resolve_tid, reject_tid) in self.index.promises(template) {
            sites.0.push((
                site,
                SiteEntry {
                    resolution: Some(SiteResolution::PromiseNew {
                        value_loc: self.facts.alloc(Ty::BOTTOM),
                        resolve_tid,
                        reject_tid,
                        executor: None,
                    }),
                    ..SiteEntry::default()
                },
            ));
        }
        self.inst_sites.push(sites);
        self.subscriptions.inputs.push(Vec::new());
        self.enqueue(inst_id);
        Ok(inst_id)
    }

    pub(super) fn unify_params(
        &mut self,
        params: &[perry_hir::ir::Param],
        args: &[Ty],
        owner: GenericOwner,
        bindings: &mut [Ty],
        reader: Option<InstanceId>,
    ) -> Result<()> {
        let mut read = |loc| {
            let (root, value) = self.facts.resolve(loc);
            if let Some(reader) = reader {
                self.subscriptions.read(reader, Fact::Storage(root));
            }
            (root, value)
        };
        for skip_open in [true, false] {
            for (param, &arg_ty) in params.iter().zip(args) {
                if skip_open && self.types.is_open_with(arg_ty, &mut read, &self.dict_reads) {
                    continue;
                }
                let resolved_arg = self.types.promise_argument(arg_ty, |loc| read(loc).1);
                let pat = self.index.locals.rows[self.index.param_local(param)?].declared;
                self.types.unify(
                    pat,
                    resolved_arg,
                    &|name| self.index.type_slots.get(&(owner, name)).copied(),
                    bindings,
                );
            }
        }
        Ok(())
    }

    fn budget_error(
        &self,
        template: TemplateId,
        next_args: &[Ty],
        caller: Option<InstanceId>,
        call_byte_offset: u32,
    ) -> Diagnostic {
        let view = self.index.view(template);
        let name = hir_compat::function_label(&view.name);
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
        let inputs = self.types.tuple(args);
        let args_key = self.arrays.argument_key(self.types, inputs);
        let key = (class, args_key);
        if let Some(&id) = self.class_index.get(&key) {
            self.share_array_layouts(inputs, self.class_insts[id].args);
            return Ok(id);
        }
        let cinfo = self.index.class(class);
        let hir_class = cinfo.declaration;

        let scope = TypeScope {
            declaration: GenericOwner::Class(class),
            arguments: inputs,
        };
        let mut field_loc_buf = Vec::with_capacity(hir_class.fields.len());
        for (field, &annotation) in hir_class.fields.iter().zip(&cinfo.field_types) {
            let ty = match field.ty {
                HirType::Any | HirType::Unknown | HirType::Function(_) => Ty::OPEN,
                _ => self.index.substitute(self.types, annotation, &[scope]),
            };
            let declared = !self.types.is_open(ty);
            let loc = self.facts.alloc(if declared { ty } else { Ty::BOTTOM });
            if declared {
                self.facts.declare(loc, ty);
            }
            field_loc_buf.push(loc);
        }
        let field_locs = self.field_locs.extend(field_loc_buf);
        let next_idx = self.class_insts.len();
        let emitted_name = if !self.class_insts.iter().any(|c| c.class == class) {
            hir_class.name.clone()
        } else {
            format!("{}__inst_{next_idx}", hir_class.name)
        };

        let cinst_id = self.class_insts.push(ClassInst {
            ty: self.types.named(self.ctx, &emitted_name),
            class,
            args: inputs,
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
