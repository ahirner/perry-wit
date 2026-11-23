//! Statement, instance, local-slot, cell, and finalization logic for solver instances.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn evaluate_instance(&mut self, inst_id: InstanceId) -> Result<()> {
        let Some(inst) = self.instances.get(inst_id.0 as usize) else {
            return Ok(());
        };
        let (template, receiver, return_loc, explicit_return, param_locs_span, args_span) = (
            inst.template,
            inst.receiver,
            inst.return_loc,
            inst.explicit_return,
            inst.param_locs,
            inst.args,
        );
        let Some(view) = self.index.view(template) else {
            return Ok(());
        };
        self.current_reads.clear();
        let mut s = EvalState::new(inst_id, receiver);

        for (p_idx, param) in view.params.iter().enumerate() {
            if let Some(default_expr) = &param.default {
                let incoming = self
                    .instance_args
                    .get(args_span.range())
                    .and_then(|sl| sl.get(p_idx))
                    .copied()
                    .unwrap_or(Ty::VOID);
                let (_, abs) = self.types.strip_nullish(incoming);
                let default_runs = incoming == Ty::VOID
                    || incoming == Ty::BOTTOM
                    || abs.is_some_and(Absence::admits_undefined);
                let def_ty = self.eval_expr(&mut s, default_expr)?;
                if default_runs
                    && let Some(&ploc) = self
                        .param_locs
                        .get(param_locs_span.range())
                        .and_then(|sl| sl.get(p_idx))
                {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, def_ty);
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

        if template == TemplateId(0) {
            for (cid, f_idx, loc) in self.static_field_locs.clone() {
                if let Some(init) = self
                    .index
                    .class(cid)
                    .and_then(|c| self.hir.classes.get(c.hir_idx))
                    .and_then(|c| c.static_fields.get(f_idx))
                    .and_then(|f| f.init.as_ref())
                {
                    let ty = self.eval_expr(&mut s, init)?;
                    let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                    self.write_loc(loc, norm);
                }
            }
        }

        let body = hir_compat::strip_synthetic_default_guards(view.body, view.params.iter());
        for _ in 0..8 {
            s.guards = Guards::default();
            s.iteration = hir_compat::Iteration::default();
            s.local_changed = false;
            s.returned = Ty::BOTTOM;
            s.has_return_value = false;
            self.eval_stmts(&mut s, body)?;
            if !s.local_changed {
                break;
            }
        }
        if s.local_changed {
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
            let mut result_ty = if s.has_return_value {
                if crate::completion::returns_optional(body) {
                    self.types.with_absences(s.returned, Absence::Undefined)
                } else {
                    s.returned
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
        (owner == self.instances.get(inst_id.0 as usize)?.template).then_some(slot)
    }

    pub(super) fn read_local_current(&self, inst_id: InstanceId, local: LocalIdx) -> Option<Ty> {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            return self
                .inst_frame_locals
                .get(inst_id.0 as usize)?
                .get(slot)
                .copied();
        }
        Some(self.locations[self.local_loc_in_instance(inst_id, local)?.0 as usize])
    }

    pub(super) fn read_local_value(&mut self, inst_id: InstanceId, local: LocalIdx) -> Option<Ty> {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            return self
                .inst_frame_locals
                .get(inst_id.0 as usize)?
                .get(slot)
                .copied();
        }
        let loc = self.local_loc_in_instance(inst_id, local)?;
        Some(self.read_loc(loc, inst_id))
    }

    pub(super) fn write_local_value(&mut self, s: &mut EvalState<'h>, local: LocalIdx, value: Ty) {
        if let Some(slot) = self.local_frame_slot_in(s.inst, local) {
            if let Some(&old) = self
                .inst_frame_locals
                .get(s.inst.0 as usize)
                .and_then(|f| f.get(slot))
            {
                let next = self.types.join_bounded(old, value);
                if next != old {
                    self.inst_frame_locals[s.inst.0 as usize][slot] = next;
                    s.local_changed = true;
                }
            }
            return;
        }
        let loc = self
            .local_loc_in_instance(s.inst, local)
            .unwrap_or_else(|| self.ensure_instance_local(s.inst, local, Ty::BOTTOM));
        self.write_loc(loc, value);
    }

    pub(super) fn set_local_fixed(&mut self, s: &mut EvalState<'h>, local: LocalIdx, value: Ty) {
        if let Some(slot) = self.local_frame_slot_in(s.inst, local) {
            if let Some(cell) = self
                .inst_frame_locals
                .get_mut(s.inst.0 as usize)
                .and_then(|f| f.get_mut(slot))
                && *cell != value
            {
                *cell = value;
                s.local_changed = true;
            }
            return;
        }
        let loc = self
            .local_loc_in_instance(s.inst, local)
            .unwrap_or_else(|| self.ensure_instance_local(s.inst, local, value));
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
        if self.index.local_module.contains(local.0 as usize) {
            return self
                .index
                .local_loc
                .get(local.0 as usize)
                .copied()
                .flatten();
        }
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(slot) = self
                .inst_locals
                .get(cur.0 as usize)
                .and_then(|sl| sl.iter().find(|s| s.local == local))
            {
                return Some(slot.loc);
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|i| i.capture_owner);
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
        if let Some(slot) = self
            .inst_locals
            .get(inst_id.0 as usize)
            .and_then(|sl| sl.iter().find(|s| s.local == local))
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
        let next = if self.types.contains_cell(val_ty, elem_loc, &self.locations)
            || self.types.has_nested_unresolved(val_ty, false)
        {
            Ty::OPEN
        } else {
            self.types
                .normalize_for_storage(self.ctx, self.decls, val_ty)
        };
        self.write_loc(elem_loc, next);
    }

    pub(super) fn write_cell_indexed(&mut self, obj_def: Ty, idx_ty: Ty, val_ty: Ty) {
        if let TyKind::Cell { loc, kind } = self.types.kind(obj_def) {
            let ok = if kind.is_array() {
                matches!(idx_ty, Ty::NUMBER | Ty::INT32)
            } else {
                idx_ty == Ty::STRING
            };
            if ok {
                self.write_cell(loc, val_ty);
            } else if idx_ty != Ty::BOTTOM {
                self.write_loc(loc, Ty::OPEN);
            }
        }
    }

    pub(super) fn resolve_cell_read(&mut self, inst_id: InstanceId, ty: Ty) -> Ty {
        let (stripped, abs) = self.types.strip_nullish(ty);
        let TyKind::Cell { loc, kind } = self.types.kind(stripped) else {
            return ty;
        };
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
        Ty::OPEN
    }

    pub(super) fn mark_written(&mut self, inst_id: InstanceId, local: LocalIdx) {
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(w) = self.inst_written.get_mut(cur.0 as usize) {
                w.insert(local.0 as usize);
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|i| i.capture_owner);
        }
    }

    pub(super) fn record_site(&mut self, inst_id: InstanceId, expr: &Expr, res: SiteResolution) {
        let Some(template) = self.instances.get(inst_id.0 as usize).map(|i| i.template) else {
            return;
        };
        let Some(local_site) = self.index.site_of(template, expr) else {
            debug_assert!(false, "unindexed site in template {}", template.0);
            return;
        };
        let old_target = self
            .inst_sites
            .get(inst_id.0 as usize)
            .and_then(|s| s.get(local_site))
            .and_then(Option::as_ref)
            .and_then(|p| self.site_target(p));
        let new_target = self.site_target(&res);
        if let Some(slot) = self
            .inst_sites
            .get_mut(inst_id.0 as usize)
            .and_then(|s| s.get_mut(local_site))
        {
            *slot = Some(res);
        }
        if old_target != new_target {
            if let Some(t) = new_target {
                self.retain_site_target(inst_id, t);
            }
            if let Some(t) = old_target {
                self.release_site_target(inst_id, t);
            }
        }
    }

    pub(super) fn eval_stmts(&mut self, s: &mut EvalState<'h>, stmts: &'h [Stmt]) -> Result<()> {
        let depth = s.guards.depth();
        for (idx, stmt) in stmts.iter().enumerate() {
            let mut handled = false;
            if let Stmt::Let { id, name, init, .. } = stmt {
                s.iteration.register_binding(*id, name, init.as_ref());
                if let Some(init) = init {
                    s.iteration.guard(*id, init);
                    if let Some(hir_compat::RecordExpr::Value(mut value)) =
                        s.iteration.expression(init)
                    {
                        while let Some(hir_compat::RecordExpr::Value(inner)) =
                            s.iteration.expression(value)
                        {
                            value = inner;
                        }
                        let ty = self.eval_expr(s, value)?;
                        if let Some(lidx) = self.index.local_idx(*id) {
                            self.write_local_value(s, lidx, ty);
                            self.mark_written(s.inst, lidx);
                        }
                        handled = true;
                    } else if matches!(init, Expr::Undefined)
                        && let Some(el) = s.iteration.element(&stmts[idx..])
                    {
                        let ty = self.eval_expr(s, el.value)?;
                        let ty = match el.available {
                            hir_compat::Availability::Known(true) => ty,
                            hir_compat::Availability::Known(false) => Ty::VOID,
                            hir_compat::Availability::Test(_) => {
                                self.types.with_absences(ty, Absence::Undefined)
                            }
                        };
                        if let Some(lidx) = self.index.local_idx(el.id) {
                            self.write_local_value(s, lidx, ty);
                            self.mark_written(s.inst, lidx);
                        }
                        handled = true;
                    }
                }
            }
            if !handled {
                self.eval_stmt(s, stmt)?;
            }
        }
        s.guards.truncate(depth);
        Ok(())
    }

    pub(super) fn eval_stmt(&mut self, s: &mut EvalState<'h>, stmt: &'h Stmt) -> Result<()> {
        match stmt {
            Stmt::Let { id, ty, init, .. } => {
                let Some(lidx) = self.index.local_idx(*id) else {
                    if let Some(init) = init {
                        self.eval_expr(s, init)?;
                    }
                    return Ok(());
                };
                let is_inferred = self.index.is_inferred_local(self.hir, *id, lidx);
                let is_deferred = self.index.local_deferred.contains(lidx.0 as usize);
                let declared = self.types.intern_hir(self.ctx, ty, !is_inferred);
                let declared = if !is_inferred {
                    self.instantiate_annotated_class(declared, 0, Some(s.inst))?
                } else {
                    declared
                };
                let initial = if !is_inferred && !self.types.is_open(declared) {
                    declared
                } else {
                    Ty::BOTTOM
                };
                if self.local_frame_slot_in(s.inst, lidx).is_none() {
                    let _ = self.ensure_instance_local(s.inst, lidx, initial);
                }
                if !is_inferred
                    && !self.types.is_open(declared)
                    && self
                        .read_local_current(s.inst, lidx)
                        .is_none_or(|c| c == Ty::BOTTOM || c == Ty::OPEN)
                {
                    self.set_local_fixed(s, lidx, declared);
                }
                let is_i64_cast = is_inferred
                    && matches!(self.types.kind(declared), TyKind::Leaf(id)
                    if matches!(id.hir(self.ctx), HirType::Named(n) if matches!(self.decls.integer_carrier(n), Some(HirType::BigInt))));
                if is_i64_cast {
                    self.write_local_value(s, lidx, declared);
                }

                if let Some(init_expr) = init {
                    let cell_kind = if matches!(init_expr, Expr::Array(items, _) if items.is_empty())
                    {
                        if is_inferred {
                            Some(CellKind::Array)
                        } else if matches!(self.types.kind(declared), TyKind::Array(e) if e == Ty::OPEN)
                        {
                            Some(CellKind::LocalArray)
                        } else {
                            None
                        }
                    } else if (is_inferred || declared == Ty::OPEN)
                        && self.index.is_empty_object_expr(self.types, init_expr)
                    {
                        Some(CellKind::Dict)
                    } else {
                        None
                    };
                    if let Some(kind) = cell_kind {
                        if matches!(
                            self.read_local_current(s.inst, lidx),
                            None | Some(Ty::BOTTOM | Ty::OPEN)
                        ) {
                            let elem_loc = self.alloc_loc(Ty::BOTTOM);
                            let cell = self.types.cell(elem_loc, kind);
                            self.write_local_value(s, lidx, cell);
                        }
                        self.mark_written(s.inst, lidx);
                        return Ok(());
                    }
                    let init_ty = self.eval_expr(s, init_expr)?;
                    if !matches!(init_expr, Expr::Undefined) {
                        self.mark_written(s.inst, lidx);
                    }
                    if matches!(self.types.kind(init_ty), TyKind::Func(_)) {
                        self.set_local_fixed(s, lidx, init_ty);
                    } else if !is_i64_cast && (is_inferred || declared == Ty::OPEN) {
                        let is_kind = |t, k| {
                            matches!(self.types.kind(t), TyKind::Leaf(id)
                            if matches!(id.hir(self.ctx), HirType::Named(n) if crate::methods::Kind::builtin(n) == Some(k)))
                        };
                        let keep_declared = (init_ty == Ty::DYNAMIC
                            || (is_kind(declared, crate::methods::Kind::Reader)
                                && !is_kind(init_ty, crate::methods::Kind::ByobReader)))
                            && !self.types.is_open(declared);
                        let stored = if keep_declared {
                            declared
                        } else if matches!(init_expr, Expr::Undefined) && is_deferred {
                            Ty::BOTTOM
                        } else {
                            self.types
                                .normalize_for_storage(self.ctx, self.decls, init_ty)
                        };
                        self.write_local_value(s, lidx, stored);
                    }
                }
            }
            Stmt::Expr(e) | Stmt::Throw(e) => {
                self.eval_expr(s, e)?;
            }
            Stmt::Return(value) => {
                s.has_return_value = true;
                let mut ty = match value {
                    Some(e) => self.eval_expr(s, e)?,
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
                if self
                    .instances
                    .get(s.inst.0 as usize)
                    .and_then(|i| self.index.view(i.template))
                    .is_some_and(|v| v.is_async)
                {
                    while let TyKind::Promise(inner) = self.types.kind(ty) {
                        ty = inner;
                    }
                }
                let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                s.returned = self.types.join(s.returned, norm);
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.eval_expr(s, condition)?;
                let otherwise = else_branch.as_deref().unwrap_or(&[]);
                let (then_reaches, else_reaches) = (
                    crate::completion::falls_through(then_branch),
                    crate::completion::falls_through(otherwise),
                );
                let branch = s.guards.branch();
                s.guards.exclude_absences(condition, true);
                self.eval_stmts(s, then_branch)?;
                let then_writes = s.guards.other_arm(branch);
                s.guards.exclude_absences(condition, false);
                self.eval_stmts(s, otherwise)?;
                if let Some(truth) = s
                    .guards
                    .join(branch, then_writes, then_reaches, else_reaches)
                {
                    s.guards.exclude_absences(condition, truth);
                    s.guards.assign_in(
                        if then_reaches { then_branch } else { otherwise },
                        [],
                        |_| None,
                    );
                }
            }
            Stmt::While { condition, body } => {
                s.guards.assign_in(body, [condition], |_| None);
                let depth = s.guards.depth();
                self.eval_expr(s, condition)?;
                s.guards.exclude_absences(condition, true);
                self.eval_stmts(s, body)?;
                s.guards.truncate(depth);
            }
            Stmt::DoWhile { condition, body } => {
                s.guards.assign_in(body, [condition], |_| None);
                self.eval_stmts(s, body)?;
                self.eval_expr(s, condition)?;
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.eval_stmt(s, init)?;
                }
                s.guards.assign_in(
                    body,
                    condition.as_ref().into_iter().chain(update.as_ref()),
                    |_| None,
                );
                let depth = s.guards.depth();
                if let Some(cond) = condition {
                    self.eval_expr(s, cond)?;
                    s.guards.exclude_absences(cond, true);
                }
                self.eval_stmts(s, body)?;
                s.guards.truncate(depth);
                if let Some(upd) = update {
                    self.eval_expr(s, upd)?;
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
                ..
            } => {
                self.eval_stmts(s, body)?;
                if let Some(catch) = catch {
                    if let Some((id, _)) = &catch.param
                        && let Some(lidx) = self.index.local_idx(*id)
                    {
                        let err_ty = self.types.named(self.ctx, "Error");
                        self.write_local_value(s, lidx, err_ty);
                    }
                    self.eval_stmts(s, &catch.body)?;
                }
                if let Some(finally) = finally {
                    self.eval_stmts(s, finally)?;
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.eval_expr(s, discriminant)?;
                for case in cases {
                    if let Some(test) = &case.test {
                        self.eval_expr(s, test)?;
                    }
                    self.eval_stmts(s, &case.body)?;
                }
            }
            Stmt::Labeled { body, .. } => self.eval_stmt(s, body)?,
            _ => {}
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> Solved {
        let mut live_instances = BitSet::default();
        let mut live_queue = Vec::new();
        for idx in 0..self.instances.len() {
            let id = InstanceId(idx as u32);
            if !self.inst_superseded.contains(idx)
                && self.is_non_site_root(id)
                && live_instances.insert(idx)
            {
                live_queue.push(id);
            }
        }
        let mut head = 0usize;
        while let Some(&caller) = live_queue.get(head) {
            head += 1;
            for res in self
                .inst_sites
                .get(caller.0 as usize)
                .into_iter()
                .flatten()
                .flatten()
            {
                if let Some(t) = self.site_target(res)
                    && live_instances.insert(t.0 as usize)
                {
                    live_queue.push(t);
                }
            }
        }

        let inst_local_tys = std::mem::take(&mut self.inst_locals)
            .iter()
            .map(|slots| {
                slots
                    .iter()
                    .filter_map(|sl| {
                        Some((
                            *self.index.local_ids.get(sl.local.0 as usize)?,
                            self.locations[sl.loc.0 as usize],
                        ))
                    })
                    .collect()
            })
            .collect();

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

        let mut class_static_field_tys: Vec<Vec<Ty>> = self
            .index
            .classes
            .iter()
            .map(|c| {
                vec![
                    Ty::OPEN;
                    self.hir
                        .classes
                        .get(c.hir_idx)
                        .map_or(0, |hc| hc.static_fields.len())
                ]
            })
            .collect();
        for &(cid, f_idx, loc) in &self.static_field_locs {
            if let Some(slot) = class_static_field_tys
                .get_mut(cid.0 as usize)
                .and_then(|f| f.get_mut(f_idx))
            {
                *slot = self.locations[loc.0 as usize];
            }
        }

        let mut class_inst_methods = vec![Vec::new(); self.class_insts.len()];
        let mut class_inst_getters = vec![Vec::new(); self.class_insts.len()];
        let mut class_inst_setters = vec![Vec::new(); self.class_insts.len()];
        let mut class_static_methods = vec![Vec::new(); self.index.classes.len()];
        let mut template_instances = vec![Vec::new(); self.index.template_count()];
        for (idx, inst) in self.instances.iter().enumerate() {
            if !live_instances.contains(idx) {
                continue;
            }
            let id = InstanceId(idx as u32);
            if let Some(list) = template_instances.get_mut(inst.template.0 as usize) {
                list.push(id);
            }
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            match view.kind {
                TemplateKind::ClassMethod { .. } => {
                    if let Some(ci) = inst.receiver {
                        class_inst_methods[ci.0 as usize].push(id);
                    }
                }
                TemplateKind::ClassGetter { getter_idx, .. } => {
                    if let Some(ci) = inst.receiver {
                        class_inst_getters[ci.0 as usize].push((getter_idx, id));
                    }
                }
                TemplateKind::ClassSetter { setter_idx, .. } => {
                    if let Some(ci) = inst.receiver {
                        class_inst_setters[ci.0 as usize].push((setter_idx, id));
                    }
                }
                TemplateKind::ClassStaticMethod { class, .. } => {
                    class_static_methods[class.0 as usize].push(id)
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
                .ty_slice(span)
                .iter()
                .any(|&m| self.type_may_be_array(m)),
            _ => false,
        }
    }
}

pub(super) fn callable_arg_mask(types: &TypeTable, args: &[Ty]) -> u64 {
    args.iter().enumerate().take(64).fold(0u64, |m, (i, &ty)| {
        if matches!(types.kind(ty), TyKind::Func(_)) {
            m | (1u64 << i)
        } else {
            m
        }
    })
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
