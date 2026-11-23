//! Statement, instance, local-slot, cell, and finalization logic for solver instances.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn evaluate_instance(&mut self, inst_id: InstanceId) -> Result<()> {
        self.evaluating_inst = Some(inst_id);
        self.current_reads.clear();
        self.current_read_list.clear();
        let res = self.evaluate_instance_inner(inst_id);
        self.evaluating_inst = None;
        self.commit_reads(inst_id);
        res
    }

    fn evaluate_instance_inner(&mut self, inst_id: InstanceId) -> Result<()> {
        let Some(inst) = self.instances.get(inst_id) else {
            return Ok(());
        };
        let (template, receiver, return_loc, has_explicit_return, param_locs_span, args_ty) = (
            inst.template,
            inst.receiver,
            inst.return_loc,
            inst.has_explicit_return,
            inst.param_locs,
            inst.args,
        );
        let Some(view) = self.index.view(template) else {
            return Ok(());
        };
        let mut s = EvalState::new(inst_id, receiver);

        for (p_idx, param) in view.params.iter().enumerate() {
            if let Some(default_expr) = &param.default {
                let incoming = self
                    .types
                    .tuple_slice(args_ty)
                    .get(p_idx)
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
                    } else if !self.types.is_open_with(
                        norm,
                        self.locations.as_slice(),
                        &self.dict_reads,
                    ) || self.types.is_open(self.locations[ploc])
                    {
                        self.write_loc(ploc, norm);
                    }
                }
            }
        }

        if template == TemplateId::INIT {
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
        self.eval_stmts(&mut s, body)?;
        if s.local_changed {
            self.enqueue(inst_id);
        }

        if template == TemplateId::INIT
            || matches!(
                view.kind,
                TemplateKind::ClassConstructor | TemplateKind::ClassSetter(_)
            )
        {
            return Ok(());
        }
        if !has_explicit_return {
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
        let owner = self.index.local_owner.get(local.index()).copied()?;
        (owner == self.instances.get(inst_id)?.template).then_some(slot)
    }

    pub(super) fn read_local_value(&mut self, inst_id: InstanceId, local: LocalIdx) -> Option<Ty> {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            return self
                .inst_frame_locals
                .get(inst_id.index())?
                .get(slot)
                .copied();
        }
        let loc = self.local_loc_in_instance(inst_id, local)?;
        Some(self.read_loc(loc, inst_id))
    }

    pub(super) fn write_local_value(&mut self, s: &mut EvalState<'h>, local: LocalIdx, value: Ty) {
        if let Some(slot) = self.local_frame_slot_in(s.inst, local) {
            if let Some(cell) = self
                .inst_frame_locals
                .get_mut(s.inst.index())
                .and_then(|f| f.get_mut(slot))
            {
                let (next, self_nested) = self.types.join_bounded(*cell, value);
                if self_nested {
                    self.self_nested_locals.insert(local.index());
                }
                if next != *cell {
                    *cell = next;
                    s.local_changed = true;
                }
            }
            return;
        }
        let loc = self
            .local_loc_in_instance(s.inst, local)
            .unwrap_or_else(|| self.ensure_instance_local(s.inst, local, Ty::BOTTOM));
        self.write_loc(loc, value);
        if self.self_nested_locs.contains(loc.index()) {
            self.self_nested_locals.insert(local.index());
        }
    }

    pub(super) fn local_loc_in_instance(
        &self,
        inst_id: InstanceId,
        local: LocalIdx,
    ) -> Option<LocId> {
        if self.index.local_module.contains(local.index()) {
            return self.index.local_loc.get(local.index()).copied().flatten();
        }
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(slot) = self
                .inst_locals
                .get(cur.index())
                .and_then(|sl| sl.iter().find(|s| s.local == local))
            {
                return Some(slot.loc);
            }
            cursor = self.instances.get(cur).and_then(|i| i.capture_owner);
        }
        None
    }

    pub(super) fn ensure_instance_local(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
        initial: Ty,
    ) -> LocId {
        if self.index.local_module.contains(local.index())
            && let Some(loc) = self.index.local_loc.get(local.index()).copied().flatten()
        {
            return loc;
        }
        if let Some(slot) = self
            .inst_locals
            .get(inst_id.index())
            .and_then(|sl| sl.iter().find(|s| s.local == local))
        {
            return slot.loc;
        }
        let loc = self.alloc_loc(initial);
        if let Some(slots) = self.inst_locals.get_mut(inst_id.index()) {
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
        let self_nested = self
            .types
            .contains_cell(val_ty, elem_loc, self.locations.as_slice());
        if self_nested {
            self.self_nested_locs.insert(elem_loc.index());
        }
        let next = if self_nested || self.types.has_nested_unresolved(val_ty, false) {
            Ty::OPEN
        } else {
            self.types
                .normalize_for_storage(self.ctx, self.decls, val_ty)
        };
        self.write_loc(elem_loc, next);
    }

    pub(super) fn push_into_cells(&mut self, stripped: Ty, vals: &[Ty]) -> bool {
        match self.types.kind(stripped) {
            TyKind::Cell { loc, kind } => {
                if kind.is_array() {
                    for &a in vals {
                        self.write_cell(loc, a);
                    }
                } else {
                    self.write_loc(loc, Ty::OPEN);
                }
                true
            }
            TyKind::Union(span) => {
                let members: Vec<Ty> = self.types.ty_slice(span).to_vec();
                let mut hit = false;
                for m in members {
                    if let TyKind::Cell { loc, kind } = self.types.kind(m)
                        && kind.is_array()
                    {
                        hit = true;
                        for &a in vals {
                            self.write_cell(loc, a);
                        }
                    }
                }
                hit
            }
            _ => false,
        }
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
        match self.types.kind(stripped) {
            TyKind::Cell { loc, kind } => {
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
                    .is_valid_dict_value(elem, self.dict_reads.contains(loc.index()))
                {
                    let obj = self.types.object(&[], Some(elem));
                    return self.types.with_absences(obj, abs);
                }
                Ty::OPEN
            }
            TyKind::Union(span) => {
                let members = self.types.ty_slice(span);
                if !members
                    .iter()
                    .any(|&m| matches!(self.types.kind(m), TyKind::Cell { .. }))
                {
                    return ty;
                }
                let members: Vec<Ty> = members.to_vec();
                let mut joined = Ty::BOTTOM;
                for m in members {
                    let r = self.resolve_cell_read(inst_id, m);
                    if r == Ty::BOTTOM {
                        return Ty::BOTTOM;
                    }
                    joined = self.types.join(joined, r);
                }
                self.types.with_absences(joined, abs)
            }
            _ => ty,
        }
    }

    pub(super) fn mark_written(&mut self, inst_id: InstanceId, local: LocalIdx) {
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(w) = self.inst_written.get_mut(cur.index()) {
                w.insert(local.index());
            }
            cursor = self.instances.get(cur).and_then(|i| i.capture_owner);
        }
    }

    pub(super) fn record_site(&mut self, inst_id: InstanceId, expr: &Expr, res: SiteResolution) {
        let Some(template) = self.instances.get(inst_id).map(|i| i.template) else {
            return;
        };
        let Some(local_site) = self.index.site_of(template, expr) else {
            debug_assert!(false, "unindexed site in template {}", template.index());
            return;
        };
        if let Some(slot) = self
            .inst_sites
            .get_mut(inst_id.index())
            .and_then(|s| s.get_mut(local_site))
        {
            *slot = Some(res);
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
                        && let Some(el) = s.iteration.element(stmts.get(idx..).unwrap_or(&[]))
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
                let is_deferred = self.index.local_deferred.contains(lidx.index());
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
                if !is_inferred && !self.types.is_open(declared) {
                    self.write_local_value(s, lidx, declared);
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
                        && !self.index.local_reassigned.contains(lidx.index())
                        && self.index.is_empty_object_expr(self.types, init_expr)
                    {
                        Some(CellKind::Dict)
                    } else {
                        None
                    };
                    if let Some(kind) = cell_kind {
                        let cell = self.alloc_expr_cell(s.inst, init_expr, kind);
                        self.write_local_value(s, lidx, cell);
                        self.mark_written(s.inst, lidx);
                        return Ok(());
                    }
                    let init_ty = self.eval_expr(s, init_expr)?;
                    if !matches!(init_expr, Expr::Undefined) {
                        self.mark_written(s.inst, lidx);
                    }
                    if matches!(self.types.kind(init_ty), TyKind::Func(_))
                        || self.types.is_tagged_func_union(
                            init_ty,
                            &self.index.capture_free_templates,
                            &self.escaping_env_templates,
                        )
                    {
                        self.write_local_value(s, lidx, init_ty);
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
                    if let Some(cur) = self.read_local_value(s.inst, lidx) {
                        self.record_coercion(s.inst, init_expr, init_ty, cur);
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
                    .get(s.inst)
                    .and_then(|i| self.index.view(i.template))
                    .is_some_and(|v| v.is_async)
                {
                    while let TyKind::Promise(inner) = self.types.kind(ty) {
                        ty = inner;
                    }
                }
                let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                self.check_escaping_func(s.inst, norm, 0, s.current_span)?;
                s.returned = self.types.join(s.returned, norm);
                self.check_escaping_func(s.inst, s.returned, 0, s.current_span)?;
                if let Some(e) = value {
                    let dst = self
                        .instances
                        .get(s.inst)
                        .and_then(|i| self.locations.get(i.return_loc).copied())
                        .map_or(s.returned, |r| self.types.join(r, s.returned));
                    self.record_coercion(s.inst, e, norm, dst);
                }
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

    pub(super) fn finish(mut self, init_inst: InstanceId) -> Solved {
        let live_instances = self.compute_live_instances();

        let inst_local_tys = std::mem::take(&mut self.inst_locals)
            .iter()
            .map(|slots| {
                slots
                    .iter()
                    .filter_map(|sl| {
                        Some((*self.index.local_ids.get(sl.local)?, self.locations[sl.loc]))
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
                .map_or(Ty::OPEN, |loc| self.locations[loc]);
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
                .get_mut(cid.index())
                .and_then(|f| f.get_mut(f_idx))
            {
                *slot = self.locations[loc];
            }
        }

        let mut class_inst_methods = vec![Vec::new(); self.class_insts.len()];
        let mut class_inst_getters = vec![Vec::new(); self.class_insts.len()];
        let mut class_inst_setters = vec![Vec::new(); self.class_insts.len()];
        let mut class_static_methods = vec![Vec::new(); self.index.classes.len()];
        for (id, inst) in self.instances.iter_enumerated() {
            if !live_instances.contains(id.index()) {
                continue;
            }
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            match view.kind {
                TemplateKind::ClassMethod => {
                    if let Some(ci) = inst.receiver.and_then(Receiver::class)
                        && let Some(list) = class_inst_methods.get_mut(ci.index())
                    {
                        list.push(id);
                    }
                }
                TemplateKind::ClassGetter(getter_idx) => {
                    if let Some(ci) = inst.receiver.and_then(Receiver::class)
                        && let Some(list) = class_inst_getters.get_mut(ci.index())
                    {
                        list.push((getter_idx, id));
                    }
                }
                TemplateKind::ClassSetter(setter_idx) => {
                    if let Some(ci) = inst.receiver.and_then(Receiver::class)
                        && let Some(list) = class_inst_setters.get_mut(ci.index())
                    {
                        list.push((setter_idx, id));
                    }
                }
                TemplateKind::ClassStaticMethod(class) => {
                    if let Some(list) = class_static_methods.get_mut(class.index()) {
                        list.push(id);
                    }
                }
                _ => {}
            }
        }

        Solved {
            init_inst,
            instances: self.instances,
            class_insts: self.class_insts,
            locations: self.locations,
            param_locs: self.param_locs,
            field_locs: self.field_locs,
            dispatch_arms: self.dispatch_arms,
            inst_local_tys,
            inst_frame_locals: self.inst_frame_locals,
            inst_sites: self.inst_sites,
            inst_coercions: self.inst_coercions,
            inst_captures: self.inst_captures,
            live_instances,
            escaping_env_templates: self.escaping_env_templates,
            boxed_locals: self.boxed_locals,
            module_local_tys,
            class_static_field_tys,
            class_inst_methods,
            class_inst_getters,
            class_inst_setters,
            class_static_methods,
            dict_reads: self.dict_reads,
            self_nested_locs: self.self_nested_locs,
            self_nested_locals: self.self_nested_locals,
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

pub(super) fn callable_arg_mask(types: &TypeTable, escaping_env: &BitSet, args: &[Ty]) -> u64 {
    args.iter().enumerate().take(64).fold(0u64, |m, (i, &ty)| {
        if let TyKind::Func(ident) = types.kind(ty)
            && !escaping_env.contains(ident.template.index())
        {
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
