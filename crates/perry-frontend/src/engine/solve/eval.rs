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
        // Converge strictly-local (non-escaping) bindings in place so intra-procedural loops
        // do not allocate global locations or churn the global worklist/CSR.
        for _local_pass in 0..8 {
            state.guards = Guards::default();
            state.iteration = hir_compat::Iteration::default();
            state.local_changed = false;
            returned = Ty::BOTTOM;
            has_return_value = false;
            self.eval_stmts(
                inst_id,
                receiver,
                body,
                &mut state,
                &mut returned,
                &mut has_return_value,
            )?;
            if !state.local_changed {
                break;
            }
        }
        if state.local_changed {
            self.enqueue(inst_id);
        }

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

    pub(super) fn local_frame_slot_in(
        &self,
        inst_id: InstanceId,
        local: LocalIdx,
    ) -> Option<usize> {
        let slot = self.index.local_frame_slot(local)?;
        let owner = self.index.local_owner.get(local.0 as usize).copied()?;
        let inst_tpl = self.instances.get(inst_id.0 as usize)?.template;
        (owner == inst_tpl).then_some(slot)
    }

    pub(super) fn read_local_current(&self, inst_id: InstanceId, local: LocalIdx) -> Option<Ty> {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            return self
                .inst_frame_locals
                .get(inst_id.0 as usize)
                .and_then(|frame| frame.get(slot))
                .copied();
        }
        let loc = self.local_loc_in_instance(inst_id, local)?;
        Some(self.locations[loc.0 as usize])
    }

    pub(super) fn read_local_value(&mut self, inst_id: InstanceId, local: LocalIdx) -> Option<Ty> {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            return self
                .inst_frame_locals
                .get(inst_id.0 as usize)
                .and_then(|frame| frame.get(slot))
                .copied();
        }
        let loc = self.local_loc_in_instance(inst_id, local)?;
        Some(self.read_loc(loc, inst_id))
    }

    pub(super) fn write_local_value(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
        value: Ty,
        state: &mut EvalState<'h>,
    ) {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            if let Some(&old) = self
                .inst_frame_locals
                .get(inst_id.0 as usize)
                .and_then(|frame| frame.get(slot))
            {
                let next = self.types.join_bounded(old, value);
                if next != old {
                    self.inst_frame_locals[inst_id.0 as usize][slot] = next;
                    state.local_changed = true;
                }
            }
            return;
        }
        let loc = self
            .local_loc_in_instance(inst_id, local)
            .unwrap_or_else(|| self.ensure_instance_local(inst_id, local, Ty::BOTTOM));
        self.write_loc(loc, value);
    }

    pub(super) fn set_local_fixed(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
        value: Ty,
        state: &mut EvalState<'h>,
    ) {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            if let Some(cell) = self
                .inst_frame_locals
                .get_mut(inst_id.0 as usize)
                .and_then(|frame| frame.get_mut(slot))
                && *cell != value
            {
                *cell = value;
                state.local_changed = true;
            }
            return;
        }
        let loc = self
            .local_loc_in_instance(inst_id, local)
            .unwrap_or_else(|| self.ensure_instance_local(inst_id, local, value));
        if let Some(cell) = self.locations.get_mut(loc.0 as usize)
            && *cell != value
        {
            *cell = value;
            self.dirty_locs.insert(loc.0 as usize);
        }
    }

    pub(super) fn local_loc_in_instance(
        &self,
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

    pub(super) fn invalidate_conservative_cell(&mut self, ty: Ty) {
        let (stripped, _) = self.types.strip_nullish(ty);
        if let TyKind::Cell { loc, kind } = self.types.kind(stripped)
            && kind.is_conservative()
        {
            self.write_loc(loc, Ty::OPEN);
        }
    }

    pub(super) fn write_cell(&mut self, elem_loc: LocId, val_ty: Ty) {
        if val_ty == Ty::BOTTOM {
            return;
        }
        if self.types.contains_cell(val_ty, elem_loc, &self.locations)
            || self.types.has_nested_unresolved(val_ty, false)
        {
            self.write_loc(elem_loc, Ty::OPEN);
        } else {
            let norm = self
                .types
                .normalize_for_storage(self.ctx, self.decls, val_ty);
            self.write_loc(elem_loc, norm);
        }
    }

    pub(super) fn write_cell_indexed(&mut self, obj_def: Ty, idx_ty: Ty, val_ty: Ty) {
        if let TyKind::Cell { loc, kind } = self.types.kind(obj_def) {
            let matches_kind = if kind.is_array() {
                matches!(idx_ty, Ty::NUMBER | Ty::INT32)
            } else {
                idx_ty == Ty::STRING
            };
            if matches_kind {
                self.write_cell(loc, val_ty);
            } else if idx_ty != Ty::BOTTOM {
                self.write_loc(loc, Ty::OPEN);
            }
        }
    }

    pub(super) fn resolve_cell_read(&mut self, inst_id: InstanceId, ty: Ty) -> Ty {
        let (stripped, abs) = self.types.strip_nullish(ty);
        if let TyKind::Cell { loc, kind } = self.types.kind(stripped) {
            let elem = self.read_loc(loc, inst_id);
            if kind.is_array() {
                if elem == Ty::BOTTOM {
                    return Ty::BOTTOM;
                }
                let arr = self.types.array(elem);
                return self.types.with_absences(arr, abs);
            }
            if self
                .types
                .is_valid_dict_value(elem, self.dict_reads.contains(loc.0 as usize))
            {
                let obj = self.types.object(&[], Some(elem));
                return self.types.with_absences(obj, abs);
            }
            return Ty::OPEN;
        }
        ty
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
        let Some(inst) = self.instances.get(inst_id.0 as usize) else {
            return;
        };
        let template = inst.template;
        let Some(local_site) = self.index.site_of(template, expr) else {
            debug_assert!(
                false,
                "unindexed site expression in template {}",
                template.0
            );
            return;
        };
        let old_target = self
            .inst_sites
            .get(inst_id.0 as usize)
            .and_then(|sites| sites.get(local_site))
            .and_then(|opt| opt.as_ref())
            .and_then(|prev| self.site_target(prev));
        let new_target = self.site_target(&res);
        if let Some(sites) = self.inst_sites.get_mut(inst_id.0 as usize)
            && let Some(slot) = sites.get_mut(local_site)
        {
            *slot = Some(res);
        }
        if old_target != new_target {
            if let Some(new_t) = new_target {
                self.retain_site_target(inst_id, new_t);
            }
            if let Some(old_t) = old_target {
                self.release_site_target(inst_id, old_t);
            }
        }
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
                            self.write_local_value(inst_id, lidx, ty, state);
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
                            self.write_local_value(inst_id, lidx, ty, state);
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
                if self.local_frame_slot_in(inst_id, lidx).is_none() {
                    let _ = self.ensure_instance_local(inst_id, lidx, initial);
                }
                if !is_inferred
                    && !self.types.is_open(declared)
                    && self
                        .read_local_current(inst_id, lidx)
                        .is_none_or(|cur| cur == Ty::BOTTOM || cur == Ty::OPEN)
                {
                    self.set_local_fixed(inst_id, lidx, declared, state);
                }

                // Preserve 64-bit WIT integer cast types (e.g. `const wrapped = total as u64`)
                // that perry-hir lowered onto `Stmt::Let.ty`.
                let is_i64_cast = is_inferred
                    && matches!(self.types.kind(declared), TyKind::Leaf(id)
                        if matches!(id.hir(self.ctx), HirType::Named(n)
                            if matches!(self.decls.integer_carrier(n), Some(HirType::BigInt))));
                if is_i64_cast {
                    self.write_local_value(inst_id, lidx, declared, state);
                }

                if let Some(init_expr) = init {
                    let is_explicit_any_array = !is_inferred
                        && matches!(self.types.kind(declared), TyKind::Array(e) if e == Ty::OPEN);
                    let open_object = is_inferred || declared == Ty::OPEN;
                    let cell_kind = if matches!(init_expr, Expr::Array(items, _) if items.is_empty())
                    {
                        if is_inferred {
                            Some(CellKind::Array)
                        } else if is_explicit_any_array {
                            Some(CellKind::LocalArray)
                        } else {
                            None
                        }
                    } else if open_object && self.index.is_empty_object_expr(self.types, init_expr)
                    {
                        Some(CellKind::Dict)
                    } else {
                        None
                    };
                    if let Some(kind) = cell_kind {
                        let cur = self.read_local_current(inst_id, lidx).unwrap_or(Ty::BOTTOM);
                        if cur == Ty::BOTTOM || cur == Ty::OPEN {
                            let elem_loc = self.alloc_loc(Ty::BOTTOM);
                            let cell = self.types.cell(elem_loc, kind);
                            self.write_local_value(inst_id, lidx, cell, state);
                        }
                        self.mark_written(inst_id, lidx);
                        return Ok(());
                    }
                    let init_ty = self.eval_expr(inst_id, receiver, init_expr, state)?;
                    if !matches!(init_expr, Expr::Undefined) {
                        self.mark_written(inst_id, lidx);
                    }
                    if matches!(self.types.kind(init_ty), TyKind::Func(_)) {
                        self.set_local_fixed(inst_id, lidx, init_ty, state);
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
                        self.write_local_value(inst_id, lidx, stored, state);
                    }
                }
            }
            Stmt::Expr(expr) | Stmt::Throw(expr) => {
                self.eval_expr(inst_id, receiver, expr, state)?;
            }
            Stmt::Return(value) => {
                *has_return_value = true;
                let mut ty = match value {
                    Some(expr) => self.eval_expr(inst_id, receiver, expr, state)?,
                    None => Ty::VOID,
                };
                if let TyKind::Cell {
                    loc,
                    kind: CellKind::Dict,
                } = self.types.kind(ty)
                {
                    self.write_loc(loc, Ty::OPEN);
                    ty = self.types.object(&[], None);
                }
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
                        self.write_local_value(inst_id, lidx, err_ty, state);
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

    pub(super) fn finish(mut self) -> Solved {
        // Compute live instances from roots through per-instance final-wave site tables.
        let mut live_instances = BitSet::default();
        let mut live_queue = Vec::new();
        for idx in 0..self.instances.len() {
            let inst_id = InstanceId(idx as u32);
            if !self.inst_superseded.contains(idx)
                && self.is_non_site_root(inst_id)
                && live_instances.insert(idx)
            {
                live_queue.push(inst_id);
            }
        }
        let mut head = 0usize;
        while let Some(&caller) = live_queue.get(head) {
            head += 1;
            if let Some(sites) = self.inst_sites.get(caller.0 as usize) {
                for opt in sites {
                    if let Some(res) = opt
                        && let Some(t) = self.site_target(res)
                        && live_instances.insert(t.0 as usize)
                    {
                        live_queue.push(t);
                    }
                }
            }
        }

        // Collect per-instance local resolved types (indexed by InstanceId).
        let inst_locals = std::mem::take(&mut self.inst_locals);
        let mut inst_local_tys: Vec<Vec<(LocalId, Ty)>> = Vec::with_capacity(inst_locals.len());
        for slots in &inst_locals {
            let mut out = Vec::with_capacity(slots.len());
            for slot in slots {
                if let Some(&hir_id) = self.index.local_ids.get(slot.local.0 as usize) {
                    out.push((hir_id, self.locations[slot.loc.0 as usize]));
                }
            }
            inst_local_tys.push(out);
        }

        // Collect module-level binding types (indexed directly by LocalIdx).
        let mut module_local_tys = vec![None; self.index.local_count()];
        for l_idx in self.index.local_module.iter() {
            let ty = self
                .index
                .local_loc
                .get(l_idx)
                .copied()
                .flatten()
                .map_or(Ty::OPEN, |loc| self.locations[loc.0 as usize]);
            if let Some(slot) = module_local_tys.get_mut(l_idx) {
                *slot = Some(ty);
            }
        }

        // Collect class static field resolved types indexed by `[ClassId][field_idx]`.
        let mut class_static_field_tys: Vec<Vec<Ty>> = self
            .index
            .class_hir_idx
            .iter()
            .map(|&hir_idx| {
                let len = self
                    .hir
                    .classes
                    .get(hir_idx)
                    .map_or(0, |c| c.static_fields.len());
                vec![Ty::OPEN; len]
            })
            .collect();
        for &(cid, f_idx, loc) in &self.static_field_locs {
            if let Some(fields) = class_static_field_tys.get_mut(cid.0 as usize)
                && let Some(slot) = fields.get_mut(f_idx)
            {
                *slot = self.locations[loc.0 as usize];
            }
        }

        // Group live instances by ClassInstId, ClassId, and TemplateId for O(1) emission.
        let mut class_inst_methods = vec![Vec::new(); self.class_insts.len()];
        let mut class_inst_getters = vec![Vec::new(); self.class_insts.len()];
        let mut class_inst_setters = vec![Vec::new(); self.class_insts.len()];
        let mut class_static_methods = vec![Vec::new(); self.index.class_hir_idx.len()];
        let mut template_instances = vec![Vec::new(); self.index.template_count()];
        for (idx, inst) in self.instances.iter().enumerate() {
            if !live_instances.contains(idx) {
                continue;
            }
            let inst_id = InstanceId(idx as u32);
            if let Some(list) = template_instances.get_mut(inst.template.0 as usize) {
                list.push(inst_id);
            }
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            match view.kind {
                TemplateKind::ClassMethod { .. } => {
                    if let Some(cinst_id) = inst.receiver
                        && let Some(list) = class_inst_methods.get_mut(cinst_id.0 as usize)
                    {
                        list.push(inst_id);
                    }
                }
                TemplateKind::ClassGetter { getter_idx, .. } => {
                    if let Some(cinst_id) = inst.receiver
                        && let Some(list) = class_inst_getters.get_mut(cinst_id.0 as usize)
                    {
                        list.push((getter_idx, inst_id));
                    }
                }
                TemplateKind::ClassSetter { setter_idx, .. } => {
                    if let Some(cinst_id) = inst.receiver
                        && let Some(list) = class_inst_setters.get_mut(cinst_id.0 as usize)
                    {
                        list.push((setter_idx, inst_id));
                    }
                }
                TemplateKind::ClassStaticMethod { class, .. } => {
                    if let Some(list) = class_static_methods.get_mut(class.0 as usize) {
                        list.push(inst_id);
                    }
                }
                _ => {}
            }
        }

        Solved {
            instances: self.instances,
            class_insts: self.class_insts,
            locations: self.locations,
            param_locs: self.param_locs,
            inst_local_tys,
            inst_frame_locals: self.inst_frame_locals,
            inst_sites: self.inst_sites,
            inst_captures: self.inst_captures,
            live_instances,
            module_local_tys,
            class_static_field_tys,
            class_inst_methods,
            class_inst_getters,
            class_inst_setters,
            class_static_methods,
            template_instances,
            dict_reads: self.dict_reads,
            deletes_array_elements: self.deletes_array_elements,
        }
    }

    pub(super) fn type_may_be_array(&self, ty: Ty) -> bool {
        match self.types.kind(ty) {
            TyKind::Array(_) | TyKind::Tuple(_) => true,
            TyKind::Cell { kind, .. } if kind.is_array() => true,
            TyKind::Union(span) => self
                .types
                .union_slice(span)
                .iter()
                .any(|&m| self.type_may_be_array(m)),
            _ => false,
        }
    }
}

pub(super) fn callable_arg_mask(types: &TypeTable, args: &[Ty]) -> u64 {
    let mut mask = 0u64;
    for (i, &ty) in args.iter().enumerate() {
        if i < 64 && matches!(types.kind(ty), TyKind::Func(_)) {
            mask |= 1u64 << i;
        }
    }
    mask
}

pub(super) fn first_type_var_name(ty: &HirType) -> Option<&str> {
    match ty {
        HirType::TypeVar(name) => Some(name.as_str()),
        HirType::Array(inner) | HirType::Promise(inner) => first_type_var_name(inner),
        HirType::Tuple(items) | HirType::Union(items) => items.iter().find_map(first_type_var_name),
        HirType::Generic { type_args, .. } => type_args.iter().find_map(first_type_var_name),
        _ => None,
    }
}

pub(super) fn constructor_init_expr(expr: &Expr) -> Option<&Expr> {
    if let Expr::New {
        class_name, args, ..
    } = expr
        && matches!(class_name.as_str(), "\0perry_Map" | "\0perry_Set")
    {
        return args.first();
    }
    let argument = match expr {
        Expr::NewDynamic { callee, args, .. }
            if matches!(&**callee, Expr::PropertyGet { object, property, .. }
                if matches!(&**object, Expr::GlobalGet(0)) && property == "Map") =>
        {
            args.first()?
        }
        Expr::MapNewFromArray(argument) | Expr::SetNewFromArray(argument) => argument,
        Expr::New {
            class_name, args, ..
        } if matches!(class_name.as_str(), "Map" | "Set") => args.first()?,
        _ => return None,
    };
    let Expr::Sequence(items) = argument else {
        return None;
    };
    let (_, rest) = items.split_first()?;
    rest.first()
        .filter(|expr| !matches!(expr, Expr::Undefined | Expr::Null))
}
