//! Statement, expression, property, and object-spread evaluation for solver instances.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn evaluate_instance(&mut self, inst_id: InstanceId) -> Result<()> {
        let Some(inst) = self.instances.get(inst_id.0 as usize) else {
            return Ok(());
        };
        let template = inst.template;
        let receiver = inst.receiver;
        let return_loc = inst.return_loc;
        let explicit_return = inst.explicit_return;
        let param_locs_span = inst.param_locs;
        let args_span = inst.args;

        let Some(view) = self.index.view(template) else {
            return Ok(());
        };
        self.current_reads.clear();
        let mut state = EvalState::default();

        // Evaluate parameter default expressions (narrowing `undefined` in frontend + lowering).
        for (p_idx, param) in view.params.iter().enumerate() {
            if let Some(default_expr) = &param.default {
                let incoming = self
                    .instance_args
                    .get(args_span.range())
                    .and_then(|slice| slice.get(p_idx))
                    .copied()
                    .unwrap_or(Ty::VOID);
                let (_, abs) = self.types.strip_nullish(incoming);
                let default_runs = incoming == Ty::VOID
                    || incoming == Ty::BOTTOM
                    || abs.is_some_and(Absence::admits_undefined);
                let def_ty = self.eval_expr(inst_id, receiver, default_expr, &mut state)?;
                if default_runs
                    && let Some(&ploc) = self
                        .param_locs
                        .get(param_locs_span.range())
                        .and_then(|slice| slice.get(p_idx))
                {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, def_ty);
                    // A default of literal `undefined` (TS optional `x?: T`) adds `undefined`;
                    // a non-undefined default narrows `undefined` away!
                    if matches!(default_expr, Expr::Undefined) {
                        if !view.is_exported {
                            self.write_loc(ploc, Ty::VOID);
                        }
                    } else if !self.types.is_open(norm) {
                        self.write_loc(ploc, norm);
                    }
                }
            }
        }

        // For ModuleInit, also evaluate class static field initializers.
        if template == TemplateId(0) {
            let static_entries: Vec<(ClassId, usize, LocId)> = self.static_field_locs.clone();
            for (cid, f_idx, loc) in static_entries {
                if let Some(&hir_idx) = self.index.class_hir_idx.get(cid.0 as usize)
                    && let Some(field) = self
                        .hir
                        .classes
                        .get(hir_idx)
                        .and_then(|c| c.static_fields.get(f_idx))
                    && let Some(init) = &field.init
                {
                    let ty = self.eval_expr(inst_id, None, init, &mut state)?;
                    let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                    self.write_loc(loc, norm);
                }
            }
        }

        let mut returned = Ty::BOTTOM;
        let mut has_return_value = false;
        let body = hir_compat::strip_synthetic_default_guards(view.body, view.params.iter());
        self.eval_stmts(
            inst_id,
            receiver,
            body,
            &mut state,
            &mut returned,
            &mut has_return_value,
        )?;

        if template == TemplateId(0)
            || matches!(
                view.kind,
                TemplateKind::ClassConstructor { .. } | TemplateKind::ClassSetter { .. }
            )
        {
            return Ok(());
        }

        if explicit_return.is_none() {
            let mut result_ty = if has_return_value {
                if crate::completion::returns_optional(body) {
                    self.types.with_absences(returned, Absence::Undefined)
                } else {
                    returned
                }
            } else {
                Ty::VOID
            };
            if view.is_async {
                while let TyKind::Promise(inner) = self.types.kind(result_ty) {
                    result_ty = inner;
                }
                result_ty = self.types.promise(result_ty);
            }
            if !self.types.is_open(result_ty) {
                self.write_loc(return_loc, result_ty);
            }
        }
        Ok(())
    }

    pub(super) fn local_loc_in_instance(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
    ) -> Option<LocId> {
        // 1. Module-level binding?
        if self.index.local_module.contains(local.0 as usize) {
            return self
                .index
                .local_loc
                .get(local.0 as usize)
                .copied()
                .flatten();
        }
        // 2. Walk capture_owner chain to find the instance owning `local`.
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(slots) = self.inst_locals.get(cur.0 as usize)
                && let Some(slot) = slots.iter().find(|s| s.local == local)
            {
                return Some(slot.loc);
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|inst| inst.capture_owner);
        }
        None
    }

    pub(super) fn ensure_instance_local(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
        initial: Ty,
    ) -> LocId {
        if self.index.local_module.contains(local.0 as usize)
            && let Some(loc) = self
                .index
                .local_loc
                .get(local.0 as usize)
                .copied()
                .flatten()
        {
            return loc;
        }
        if let Some(slots) = self.inst_locals.get(inst_id.0 as usize)
            && let Some(slot) = slots.iter().find(|s| s.local == local)
        {
            return slot.loc;
        }
        let loc = self.alloc_loc(initial);
        if let Some(slots) = self.inst_locals.get_mut(inst_id.0 as usize) {
            slots.push(InstanceLocalSlot { local, loc });
        }
        loc
    }

    pub(super) fn lookup_agg(
        &self,
        inst_id: InstanceId,
        local: LocalIdx,
    ) -> Option<(LocId, AggregateKind)> {
        if self.index.local_module.contains(local.0 as usize) {
            return self.module_aggs.get(local.0 as usize).copied().flatten();
        }
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(aggs) = self.inst_aggs.get(cur.0 as usize)
                && let Some(slot) = aggs.iter().find(|s| s.local == local)
            {
                return Some((slot.loc, slot.kind));
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|inst| inst.capture_owner);
        }
        None
    }

    pub(super) fn bind_agg(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
        kind: AggregateKind,
    ) -> LocId {
        if let Some((loc, _)) = self.lookup_agg(inst_id, local) {
            return loc;
        }
        let loc = self.alloc_loc(Ty::BOTTOM);
        if self.index.local_module.contains(local.0 as usize) {
            if let Some(slot) = self.module_aggs.get_mut(local.0 as usize) {
                *slot = Some((loc, kind));
            }
        } else if let Some(aggs) = self.inst_aggs.get_mut(inst_id.0 as usize) {
            aggs.push(AggregateSlot { local, loc, kind });
        }
        loc
    }

    pub(super) fn alias_agg(
        &mut self,
        inst_id: InstanceId,
        dst: LocalIdx,
        loc: LocId,
        kind: AggregateKind,
    ) {
        if self.index.local_module.contains(dst.0 as usize) {
            if let Some(slot) = self.module_aggs.get_mut(dst.0 as usize) {
                *slot = Some((loc, kind));
            }
        } else if let Some(aggs) = self.inst_aggs.get_mut(inst_id.0 as usize) {
            if let Some(existing) = aggs.iter_mut().find(|s| s.local == dst) {
                existing.loc = loc;
                existing.kind = kind;
            } else {
                aggs.push(AggregateSlot {
                    local: dst,
                    loc,
                    kind,
                });
            }
        }
    }

    pub(super) fn is_agg_unresolved_value(&self, ty: Ty, nested: bool) -> bool {
        match self.types.kind(ty) {
            TyKind::Bottom | TyKind::Open | TyKind::Dynamic | TyKind::TypeVar(_) => true,
            TyKind::Array(inner) => self.is_agg_unresolved_value(inner, true),
            TyKind::Tuple(span) => {
                let items = self.types.tuple_slice(span);
                let non_uniform = items.windows(2).any(|w| w[0] != w[1]);
                (nested && non_uniform)
                    || items.iter().any(|&t| self.is_agg_unresolved_value(t, true))
            }
            TyKind::Union(span) => {
                let members = self.types.union_slice(span);
                if members
                    .iter()
                    .any(|&t| self.is_agg_unresolved_value(t, nested))
                {
                    return true;
                }
                if nested {
                    let defined_count = members
                        .iter()
                        .filter(|&&m| m != Ty::NULL && m != Ty::VOID)
                        .count();
                    defined_count > 1
                } else {
                    false
                }
            }
            TyKind::Object { props, index } => {
                index.is_some_and(|t| self.is_agg_unresolved_value(t, true))
                    || self
                        .types
                        .object_slice(props)
                        .iter()
                        .any(|p| self.is_agg_unresolved_value(p.ty, true))
            }
            _ => false,
        }
    }

    pub(super) fn write_agg_value(&mut self, agg_loc: LocId, val_ty: Ty) {
        if val_ty == Ty::BOTTOM {
            return;
        }
        if self.is_agg_unresolved_value(val_ty, false) {
            self.write_loc(agg_loc, Ty::OPEN);
        } else {
            let norm = self
                .types
                .normalize_for_storage(self.ctx, self.decls, val_ty);
            self.write_loc(agg_loc, norm);
        }
    }

    pub(super) fn invalidate_agg_local(&mut self, inst_id: InstanceId, id: LocalId) {
        if let Some(lidx) = self.index.local_idx(id)
            && let Some((agg_loc, _)) = self.lookup_agg(inst_id, lidx)
        {
            self.write_loc(agg_loc, Ty::OPEN);
        }
    }

    pub(super) fn invalidate_agg_expr(&mut self, inst_id: InstanceId, expr: &Expr) {
        if let Expr::LocalGet(id) = expr {
            self.invalidate_agg_local(inst_id, *id);
        }
    }

    pub(super) fn project_agg_from_elem(
        &mut self,
        agg_loc: LocId,
        agg_kind: AggregateKind,
        elem: Ty,
    ) -> Option<Ty> {
        if elem == Ty::BOTTOM {
            return None;
        }
        match agg_kind {
            AggregateKind::Array => {
                if self.types.is_open(elem) || elem == Ty::DYNAMIC {
                    Some(Ty::OPEN)
                } else {
                    Some(self.types.array(elem))
                }
            }
            AggregateKind::Dictionary => {
                let primitive = matches!(
                    elem,
                    Ty::NUMBER | Ty::INT32 | Ty::STRING | Ty::BOOLEAN | Ty::BIGINT
                );
                let valid = !self.types.is_open(elem)
                    && elem != Ty::DYNAMIC
                    && !matches!(self.types.kind(elem), TyKind::Union(_))
                    && (!self.agg_element_reads.contains(agg_loc.0 as usize) || primitive);
                if valid {
                    Some(self.types.object(&mut [], Some(elem)))
                } else {
                    Some(self.types.object(&mut [], None))
                }
            }
        }
    }

    pub(super) fn project_agg_type(
        &mut self,
        inst_id: InstanceId,
        agg_loc: LocId,
        agg_kind: AggregateKind,
    ) -> Option<Ty> {
        let elem = self.read_loc(agg_loc, inst_id);
        self.project_agg_from_elem(agg_loc, agg_kind, elem)
    }

    pub(super) fn mark_written(&mut self, inst_id: InstanceId, local: LocalIdx) {
        // Walk up capture_owner if `local` belongs to an enclosing instance (e.g. S3 `set("hi")`).
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(written) = self.inst_written.get_mut(cur.0 as usize) {
                written.insert(local.0 as usize);
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|inst| inst.capture_owner);
        }
    }

    pub(super) fn record_site(&mut self, inst_id: InstanceId, expr: &Expr, res: SiteResolution) {
        let addr = std::ptr::from_ref(expr) as usize;
        self.sites.push((inst_id, addr, res));
    }

    pub(super) fn eval_stmts(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        stmts: &'h [Stmt],
        state: &mut EvalState<'h>,
        returned: &mut Ty,
        has_return_value: &mut bool,
    ) -> Result<()> {
        let depth = state.guards.depth();
        for (idx, stmt) in stmts.iter().enumerate() {
            let mut handled_let = false;
            if let Stmt::Let { id, name, init, .. } = stmt {
                state.iteration.register_binding(*id, name, init.as_ref());
                if let Some(init) = init {
                    state.iteration.guard(*id, init);
                    if let Some(hir_compat::RecordExpr::Value(mut value)) =
                        state.iteration.expression(init)
                    {
                        while let Some(hir_compat::RecordExpr::Value(inner)) =
                            state.iteration.expression(value)
                        {
                            value = inner;
                        }
                        let ty = self.eval_expr(inst_id, receiver, value, state)?;
                        if let Some(lidx) = self.index.local_idx(*id) {
                            let loc = self.ensure_instance_local(inst_id, lidx, Ty::BOTTOM);
                            self.write_loc(loc, ty);
                            self.mark_written(inst_id, lidx);
                        }
                        handled_let = true;
                    } else if matches!(init, Expr::Undefined)
                        && let Some(element) = state.iteration.element(&stmts[idx..])
                    {
                        let ty = self.eval_expr(inst_id, receiver, element.value, state)?;
                        let ty = match element.available {
                            hir_compat::Availability::Known(true) => ty,
                            hir_compat::Availability::Known(false) => Ty::VOID,
                            hir_compat::Availability::Test(_) => {
                                self.types.with_absences(ty, Absence::Undefined)
                            }
                        };
                        if let Some(lidx) = self.index.local_idx(element.id) {
                            let loc = self.ensure_instance_local(inst_id, lidx, Ty::BOTTOM);
                            self.write_loc(loc, ty);
                            self.mark_written(inst_id, lidx);
                        }
                        handled_let = true;
                    }
                }
            }
            if !handled_let {
                self.eval_stmt(inst_id, receiver, stmt, state, returned, has_return_value)?;
            }
        }
        state.guards.truncate(depth);
        Ok(())
    }

    pub(super) fn eval_stmt(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        stmt: &'h Stmt,
        state: &mut EvalState<'h>,
        returned: &mut Ty,
        has_return_value: &mut bool,
    ) -> Result<()> {
        match stmt {
            Stmt::Let { id, ty, init, .. } => {
                let Some(lidx) = self.index.local_idx(*id) else {
                    if let Some(init) = init {
                        self.eval_expr(inst_id, receiver, init, state)?;
                    }
                    return Ok(());
                };
                let is_inferred = self.index.local_inferred.contains(lidx.0 as usize)
                    || !self.hir.local_source_spans.contains_key(id);
                let is_deferred = self.index.local_deferred.contains(lidx.0 as usize);
                let declared = self.types.intern_hir(self.ctx, ty, !is_inferred);
                let declared = if !is_inferred {
                    self.instantiate_annotated_class(declared, 0, Some(inst_id))?
                } else {
                    declared
                };
                let initial = if is_deferred && is_inferred {
                    Ty::BOTTOM
                } else if !is_inferred && !self.types.is_open(declared) {
                    declared
                } else {
                    Ty::BOTTOM
                };
                let loc = self.ensure_instance_local(inst_id, lidx, initial);
                if !is_inferred
                    && !self.types.is_open(declared)
                    && let Some(slot) = self.locations.get_mut(loc.0 as usize)
                    && *slot != declared
                {
                    *slot = declared;
                    self.dirty_locs.insert(loc.0 as usize);
                }

                // Preserve 64-bit WIT integer cast types (e.g. `const wrapped = total as u64`)
                // that perry-hir lowered onto `Stmt::Let.ty`.
                let is_i64_cast = is_inferred
                    && matches!(self.types.kind(declared), TyKind::Leaf(id)
                        if matches!(id.hir(self.ctx), HirType::Named(n)
                            if matches!(self.decls.integer_carrier(n), Some(HirType::BigInt))));
                if is_i64_cast {
                    self.write_loc(loc, declared);
                }

                if let Some(init_expr) = init {
                    let open_array = is_inferred
                        || matches!(self.types.kind(declared), TyKind::Array(e) if e == Ty::OPEN);
                    let open_object = is_inferred || declared == Ty::OPEN;
                    if open_array && matches!(init_expr, Expr::Array(items, _) if items.is_empty())
                    {
                        let agg_loc = self.bind_agg(inst_id, lidx, AggregateKind::Array);
                        if let Some(target) =
                            self.project_agg_type(inst_id, agg_loc, AggregateKind::Array)
                        {
                            self.set_loc_exact(loc, target);
                        }
                        self.mark_written(inst_id, lidx);
                        return Ok(());
                    }
                    if open_object && self.index.is_empty_object_expr(init_expr) {
                        let agg_loc = self.bind_agg(inst_id, lidx, AggregateKind::Dictionary);
                        if let Some(target) =
                            self.project_agg_type(inst_id, agg_loc, AggregateKind::Dictionary)
                        {
                            self.set_loc_exact(loc, target);
                        }
                        self.mark_written(inst_id, lidx);
                        return Ok(());
                    }
                    if let Expr::LocalGet(src_id) = init_expr
                        && let Some(src_lidx) = self.index.local_idx(*src_id)
                        && let Some((agg_loc, agg_kind)) = self.lookup_agg(inst_id, src_lidx)
                    {
                        self.alias_agg(inst_id, lidx, agg_loc, agg_kind);
                        if let Some(target) = self.project_agg_type(inst_id, agg_loc, agg_kind) {
                            self.set_loc_exact(loc, target);
                        }
                        self.mark_written(inst_id, lidx);
                        return Ok(());
                    }
                    let init_ty = self.eval_expr(inst_id, receiver, init_expr, state)?;
                    if !matches!(init_expr, Expr::Undefined) {
                        self.mark_written(inst_id, lidx);
                    }
                    if matches!(self.types.kind(init_ty), TyKind::Func(_)) {
                        if let Some(slot) = self.locations.get_mut(loc.0 as usize)
                            && *slot != init_ty
                        {
                            *slot = init_ty;
                            self.dirty_locs.insert(loc.0 as usize);
                        }
                    } else if !is_i64_cast && (is_inferred || declared == Ty::OPEN) {
                        let is_non_byob_reader_cast = matches!(
                            self.types.kind(declared),
                            TyKind::Leaf(id)
                                if matches!(
                                    id.hir(self.ctx),
                                    HirType::Named(n)
                                        if crate::methods::Kind::builtin(n)
                                            == Some(crate::methods::Kind::Reader)
                                )
                        ) && !matches!(
                            self.types.kind(init_ty),
                            TyKind::Leaf(id)
                                if matches!(
                                    id.hir(self.ctx),
                                    HirType::Named(n)
                                        if crate::methods::Kind::builtin(n)
                                            == Some(crate::methods::Kind::ByobReader)
                                )
                        );
                        let stored = if (init_ty == Ty::DYNAMIC || is_non_byob_reader_cast)
                            && !self.types.is_open(declared)
                        {
                            // Preserve concrete target type on dynamic casts (`x as T`) and
                            // non-BYOB reader casts (`3 as ReadableStreamDefaultReader`).
                            declared
                        } else if matches!(init_expr, Expr::Undefined) && is_deferred {
                            Ty::BOTTOM
                        } else {
                            self.types
                                .normalize_for_storage(self.ctx, self.decls, init_ty)
                        };
                        self.write_loc(loc, stored);
                    }
                }
            }
            Stmt::Expr(expr) | Stmt::Throw(expr) => {
                self.eval_expr(inst_id, receiver, expr, state)?;
            }
            Stmt::Return(value) => {
                *has_return_value = true;
                if let Some(Expr::LocalGet(id)) = value
                    && let Some(lidx) = self.index.local_idx(*id)
                    && let Some((agg_loc, AggregateKind::Dictionary)) =
                        self.lookup_agg(inst_id, lidx)
                {
                    self.write_loc(agg_loc, Ty::OPEN);
                }
                let mut ty = match value {
                    Some(expr) => self.eval_expr(inst_id, receiver, expr, state)?,
                    None => Ty::VOID,
                };
                let is_async = self
                    .instances
                    .get(inst_id.0 as usize)
                    .and_then(|i| self.index.view(i.template))
                    .is_some_and(|v| v.is_async);
                if is_async {
                    while let TyKind::Promise(inner) = self.types.kind(ty) {
                        ty = inner;
                    }
                }
                let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                *returned = self.types.join(*returned, norm);
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.eval_expr(inst_id, receiver, condition, state)?;
                let otherwise = else_branch.as_deref().unwrap_or(&[]);
                let then_reaches = crate::completion::falls_through(then_branch);
                let else_reaches = crate::completion::falls_through(otherwise);
                let branch = state.guards.branch();
                state.guards.exclude_absences(condition, true);
                self.eval_stmts(
                    inst_id,
                    receiver,
                    then_branch,
                    state,
                    returned,
                    has_return_value,
                )?;
                let then_writes = state.guards.other_arm(branch);
                state.guards.exclude_absences(condition, false);
                self.eval_stmts(
                    inst_id,
                    receiver,
                    otherwise,
                    state,
                    returned,
                    has_return_value,
                )?;
                if let Some(truth) =
                    state
                        .guards
                        .join(branch, then_writes, then_reaches, else_reaches)
                {
                    state.guards.exclude_absences(condition, truth);
                    state.guards.assign_in(
                        if then_reaches { then_branch } else { otherwise },
                        [],
                        |_| None,
                    );
                }
            }
            Stmt::While { condition, body } => {
                state.guards.assign_in(body, [condition], |_| None);
                let depth = state.guards.depth();
                self.eval_expr(inst_id, receiver, condition, state)?;
                state.guards.exclude_absences(condition, true);
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                state.guards.truncate(depth);
            }
            Stmt::DoWhile { condition, body } => {
                state.guards.assign_in(body, [condition], |_| None);
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                self.eval_expr(inst_id, receiver, condition, state)?;
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.eval_stmt(inst_id, receiver, init, state, returned, has_return_value)?;
                }
                state.guards.assign_in(
                    body,
                    condition.as_ref().into_iter().chain(update.as_ref()),
                    |_| None,
                );
                let depth = state.guards.depth();
                if let Some(cond) = condition {
                    self.eval_expr(inst_id, receiver, cond, state)?;
                    state.guards.exclude_absences(cond, true);
                }
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                state.guards.truncate(depth);
                if let Some(upd) = update {
                    self.eval_expr(inst_id, receiver, upd, state)?;
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
                ..
            } => {
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                if let Some(catch) = catch {
                    if let Some((id, _)) = &catch.param
                        && let Some(lidx) = self.index.local_idx(*id)
                    {
                        let err_ty = self.types.named(self.ctx, "Error");
                        let loc = self.ensure_instance_local(inst_id, lidx, err_ty);
                        self.write_loc(loc, err_ty);
                    }
                    self.eval_stmts(
                        inst_id,
                        receiver,
                        &catch.body,
                        state,
                        returned,
                        has_return_value,
                    )?;
                }
                if let Some(finally) = finally {
                    self.eval_stmts(
                        inst_id,
                        receiver,
                        finally,
                        state,
                        returned,
                        has_return_value,
                    )?;
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.eval_expr(inst_id, receiver, discriminant, state)?;
                for case in cases {
                    if let Some(test) = &case.test {
                        self.eval_expr(inst_id, receiver, test, state)?;
                    }
                    self.eval_stmts(
                        inst_id,
                        receiver,
                        &case.body,
                        state,
                        returned,
                        has_return_value,
                    )?;
                }
            }
            Stmt::Labeled { body, .. } => {
                self.eval_stmt(inst_id, receiver, body, state, returned, has_return_value)?;
            }
            _ => {}
        }
        Ok(())
    }
}
