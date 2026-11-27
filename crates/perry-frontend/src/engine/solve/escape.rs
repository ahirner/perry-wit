//! Escaping closures, capture collapsing, function coercions, and error annotation.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn closure_identity(
        &mut self,
        owner: InstanceId,
        template: TemplateId,
    ) -> FuncIdentity {
        let captures = self.index.template_captures(template);
        let lexical_this = self.index.view(template).captures_this;
        let receiver = self.instances[owner].receiver.filter(|_| lexical_this);
        let capture_owner = if captures.is_empty() && receiver.is_none() {
            CaptureOwner::None
        } else {
            let instance = &self.instances[owner];
            let owner_template = instance.template;
            let nested = self.collapsed_closures.contains(template.index())
                || self
                    .types
                    .contains_func_template(instance.args, template, None)
                || captures.iter().any(|&id| {
                    self.index
                        .local_idx(id)
                        .and_then(|local| self.read_local_value(owner, local))
                        .is_some_and(|ty| {
                            self.types.contains_func_template(
                                ty,
                                template,
                                Some(CaptureOwner::Instance(owner)),
                            )
                        })
                });
            if nested {
                self.collapse_closure(template, owner_template);
                CaptureOwner::Template(owner_template, receiver)
            } else {
                CaptureOwner::Instance(owner)
            }
        };
        FuncIdentity {
            template,
            capture_owner,
        }
    }

    pub(super) fn collapse_closure(&mut self, tid: TemplateId, owner_tid: TemplateId) {
        if !self.collapsed_closures.insert(tid.index()) {
            return;
        }
        let caps: Vec<LocalId> = self.index.template_captures(tid).to_vec();
        for cap_id in caps {
            let Some(lidx) = self.index.local_idx(cap_id) else {
                continue;
            };
            let local = self.index.locals[lidx];
            if local.flags.contains(LocalFlags::MODULE) {
                continue;
            }
            let cloc =
                *self.collapsed_locals[lidx].get_or_insert_with(|| self.facts.alloc(Ty::BOTTOM));
            for (id, inst) in self.instances.iter_enumerated() {
                if inst.template == local.owner
                    && !self.inst_superseded.contains(id.index())
                    && let Some(loc) =
                        CaptureOwner::Instance(id).location(&self.instances, local, Some(cloc))
                {
                    self.facts.write(self.types, cloc, self.facts[loc]);
                }
            }
        }
        let owners: Vec<InstanceId> = self
            .instances
            .iter_enumerated()
            .filter_map(|(id, inst)| {
                (!self.inst_superseded.contains(id.index()) && inst.template == owner_tid)
                    .then_some(id)
            })
            .collect();
        for id in owners {
            self.enqueue(id);
        }
    }

    pub(super) fn is_escaping_env(&mut self, tid: TemplateId, reader: InstanceId) -> bool {
        self.subscriptions.read(reader, Fact::Escape(tid));
        self.facts.escaping.contains(tid.index())
    }

    fn track_function_escapes(&mut self, ty: Ty, reader: InstanceId) {
        for id in self.types.func_members(&ty) {
            self.subscriptions.read(reader, Fact::Escape(id.template));
        }
    }

    pub(super) fn is_tagged_func_union_in(&mut self, ty: Ty, reader: InstanceId) -> bool {
        if !matches!(self.types.kind(ty), TyKind::Union(_)) {
            return false;
        }
        self.track_function_escapes(ty, reader);
        self.types.is_tagged_func_union(
            ty,
            &self.index.capture_free_templates,
            &self.facts.escaping,
        )
    }

    pub(super) fn mark_escaping_env(&mut self, ty: Ty) {
        for ident in self.types.func_members(&ty) {
            let tid = ident.template;
            if ident.capture_owner.is_none()
                && self.index.capture_free_templates.contains(tid.index())
            {
                continue;
            }
            if !self.facts.escaping.insert(tid.index()) {
                continue;
            }
            self.facts.dirty.insert(Fact::Escape(tid));
            for &cap_id in self.index.template_captures(tid) {
                if let Some(lidx) = self.index.local_idx(cap_id) {
                    let flags = self.index.locals[lidx].flags;
                    if !flags.contains(LocalFlags::MODULE)
                        && flags.intersects(LocalFlags::BOXED_TRIGGER)
                    {
                        self.boxed_locals.insert(lidx.index());
                    }
                }
            }
        }
    }

    pub(super) fn record_site_coercion(
        &mut self,
        inst_id: InstanceId,
        expr: &Expr,
        c: FuncCoercion,
    ) {
        if let Some(site) = self.index.expressions.index(expr) {
            self.inst_sites[inst_id].entry(site).or_default().coercion = Some(c);
        } else {
            debug_assert!(false, "unindexed coercion site");
        }
    }

    pub(super) fn clear_site(&mut self, inst: InstanceId, expr: &Expr) {
        if let Some(site) = self.index.expressions.index(expr)
            && let Some(entry) = self.inst_sites[inst].get_mut(&site)
        {
            entry.resolution = None;
        }
    }

    pub(super) fn coerce_array_write(
        &mut self,
        inst_id: InstanceId,
        arr_def: Ty,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        for &at in arg_tys {
            self.mark_escaping_env(at);
        }
        let array = self.resolve_cell_read(inst_id, arr_def);
        let TyKind::Array(elem_ty) = self.types.kind(array) else {
            return;
        };
        if self
            .types
            .is_zero_sized_func(elem_ty, &self.index.capture_free_templates)
        {
            for a in args {
                self.record_site_coercion(inst_id, a, FuncCoercion::Value(ValueCoercion::Zero));
            }
        } else if self.is_tagged_func_union_in(elem_ty, inst_id) {
            for (a, &at) in args.iter().zip(arg_tys) {
                self.record_coercion(inst_id, a, at, elem_ty);
            }
        }
    }

    pub(super) fn record_coercion(
        &mut self,
        inst_id: InstanceId,
        expr: &Expr,
        src_ty: Ty,
        dst_ty: Ty,
    ) {
        if src_ty == Ty::BOTTOM || !matches!(self.types.kind(dst_ty), TyKind::Union(_)) {
            return;
        }
        let inner = match expr {
            Expr::Sequence(parts) if self.hir.locations.operator_span(parts).is_some() => {
                parts.last().unwrap_or(expr)
            }
            _ => expr,
        };
        if matches!(inner, Expr::Conditional { .. } | Expr::Logical { .. }) {
            return;
        }
        self.track_function_escapes(dst_ty, inst_id);
        self.track_function_escapes(src_ty, inst_id);
        if let Some(conversion) = ValueCoercion::into_union(
            self.types,
            src_ty,
            dst_ty,
            &self.index.capture_free_templates,
            &self.facts.escaping,
        ) {
            self.record_site_coercion(inst_id, expr, FuncCoercion::Value(conversion));
        }
    }

    fn annotate_inst_error(
        &self,
        inst_id: InstanceId,
        mut err: Diagnostic,
        byte_offset: u32,
        span: Option<swc_common::Span>,
    ) -> Diagnostic {
        if hir_compat::has_source_offset(byte_offset) {
            err = self.hir.locations.annotate_call(err, byte_offset);
        } else if let Some(sp) = span {
            err = self.hir.locations.annotate(err, sp);
        } else if let Some(sp) = self
            .hir
            .locations
            .function_span(&self.index.view(self.instances[inst_id].template).name)
        {
            err = err.with_span(sp);
        }
        attach_inst_chain(
            &self.instances,
            self.index,
            &self.hir.locations,
            err,
            inst_id,
        )
    }

    pub(super) fn annotate_expr_error(
        &self,
        s: &EvalState<'h>,
        err: Diagnostic,
        byte_offset: u32,
    ) -> Diagnostic {
        self.annotate_inst_error(s.inst, err, byte_offset, s.current_span)
    }

    pub(super) fn check_escaping_func(
        &mut self,
        inst_id: InstanceId,
        ty: Ty,
        byte_offset: u32,
        span: Option<swc_common::Span>,
    ) -> Result<()> {
        self.mark_escaping_env(ty);
        if let TyKind::Union(u_span) = self.types.kind(ty)
            && self
                .types
                .ty_slice(u_span)
                .iter()
                .any(|&m| matches!(self.types.kind(m), TyKind::Func(_)))
            && !self.is_tagged_func_union_in(ty, inst_id)
        {
            let err = Diagnostic::unsupported(
                "joining function values with non-function values is not supported yet",
            );
            return Err(self.annotate_inst_error(inst_id, err, byte_offset, span));
        }
        Ok(())
    }
}
