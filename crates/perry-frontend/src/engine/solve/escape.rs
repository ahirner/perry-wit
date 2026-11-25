//! Escaping closures, capture collapsing, function coercions, and error annotation.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
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
            let cloc = match self.collapsed_local_locs[lidx] {
                Some(loc) => loc,
                None => {
                    let loc = self.alloc_loc(Ty::BOTTOM);
                    self.collapsed_local_locs[lidx] = Some(loc);
                    loc
                }
            };
            let cap_owner_tid = local.owner;
            let seed_tys: Vec<Ty> = self
                .instances
                .iter_enumerated()
                .filter_map(|(id, inst)| {
                    if self.inst_superseded.contains(id.index()) || inst.template != cap_owner_tid {
                        return None;
                    }
                    if let Some(slot) = self.local_frame_slot_in(id, lidx) {
                        self.inst_frame_locals[id].get(slot).copied()
                    } else {
                        self.inst_locals[id]
                            .iter()
                            .find(|s| s.local == lidx)
                            .map(|s| self.locations[s.loc])
                    }
                })
                .collect();
            for ty in seed_tys {
                self.write_loc(cloc, ty);
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
        let loc = self.template_escape_locs[tid];
        self.read_loc(loc, reader) != Ty::BOTTOM
    }

    pub(super) fn is_tagged_func_union_in(&mut self, ty: Ty, reader: InstanceId) -> bool {
        if let TyKind::Union(span) = self.types.kind(ty) {
            let members: Vec<Ty> = self.types.ty_slice(span).to_vec();
            for m in members {
                if let TyKind::Func(ident) = self.types.kind(m) {
                    let _ = self.is_escaping_env(ident.template, reader);
                }
            }
        }
        self.types.is_tagged_func_union(
            ty,
            &self.index.capture_free_templates,
            &self.escaping_env_templates,
        )
    }

    pub(super) fn mark_escaping_env(&mut self, _caller: InstanceId, ty: Ty) {
        let members: Vec<_> = self
            .types
            .func_members(&ty)
            .filter(|ident| {
                !ident.capture_owner.is_none()
                    || !self
                        .index
                        .capture_free_templates
                        .contains(ident.template.index())
            })
            .collect();
        for ident in members {
            let tid = ident.template;
            if !self.escaping_env_templates.insert(tid.index()) {
                continue;
            }
            let loc = self.template_escape_locs[tid];
            self.write_loc(loc, Ty::BOOLEAN);
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
        let template = self.instances[inst_id].template;
        let Some(local_site) = self.index.site_of(template, expr) else {
            debug_assert!(
                false,
                "unindexed coercion site in template {}",
                template.index()
            );
            return;
        };
        if let Some(slot) = self.inst_sites[inst_id].get_mut(local_site) {
            slot.coercion = Some(c);
        }
    }

    pub(super) fn clear_site(&mut self, inst_id: InstanceId, expr: &Expr) {
        let template = self.instances[inst_id].template;
        let Some(local_site) = self.index.site_of(template, expr) else {
            return;
        };
        if let Some(slot) = self.inst_sites[inst_id].get_mut(local_site) {
            slot.resolution = None;
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
            self.mark_escaping_env(inst_id, at);
        }
        let elem_ty = match self.types.kind(arr_def) {
            TyKind::Cell { loc, kind } if kind.is_array() => Some(self.read_loc(loc, inst_id)),
            TyKind::Array(elem) => Some(elem),
            _ => None,
        };
        let Some(elem_ty) = elem_ty else { return };
        if self
            .types
            .is_zero_sized_func(elem_ty, &self.index.capture_free_templates)
        {
            for a in args {
                self.record_site_coercion(inst_id, a, FuncCoercion::Zero);
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
        if src_ty == Ty::BOTTOM || dst_ty == Ty::BOTTOM {
            return;
        }
        if !self.is_tagged_func_union_in(dst_ty, inst_id) {
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
        if self
            .types
            .is_zero_sized_func(src_ty, &self.index.capture_free_templates)
            && let TyKind::Func(ident) = self.types.kind(src_ty)
        {
            self.record_site_coercion(inst_id, expr, FuncCoercion::Tag(ident, dst_ty));
            return;
        }
        if let TyKind::Func(ident) = self.types.kind(src_ty)
            && self.is_escaping_env(ident.template, inst_id)
        {
            self.record_site_coercion(inst_id, expr, FuncCoercion::BoxEnv(ident, dst_ty));
            return;
        }
        if self.is_tagged_func_union_in(src_ty, inst_id)
            && self.types.strip_nullish(src_ty).0 != self.types.strip_nullish(dst_ty).0
        {
            self.record_site_coercion(
                inst_id,
                expr,
                FuncCoercion::Widen {
                    from: src_ty,
                    to: dst_ty,
                },
            );
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
            .function_span(self.index.view(self.instances[inst_id].template).name)
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
        self.mark_escaping_env(inst_id, ty);
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
