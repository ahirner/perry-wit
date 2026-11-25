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
        let inst = &self.instances[inst_id];
        let is_witness = inst.is_witness;
        let (template, receiver, return_loc, has_explicit_return, param_locs_span, args_ty) = (
            inst.template,
            inst.receiver,
            inst.return_loc,
            inst.has_explicit_return,
            inst.param_locs,
            inst.args,
        );
        let view = self.index.view(template);
        if matches!(
            view.kind,
            TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
        ) {
            return Ok(());
        }
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
                if default_runs && let Some(&ploc) = self.param_locs[param_locs_span].get(p_idx) {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, def_ty);
                    if matches!(default_expr, Expr::Undefined) {
                        if !is_witness {
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
                    .hir
                    .bodies
                    .classes
                    .get(self.index.class(cid).hir_idx)
                    .and_then(|c| c.static_fields.get(f_idx))
                    .and_then(|f| f.init.as_ref())
                {
                    let ty = self.eval_expr(&mut s, init)?;
                    let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                    self.write_loc(loc, norm);
                }
            }
        }
        if view.kind == TemplateKind::FieldInitializers {
            let Some(Receiver::Class(id)) = receiver else {
                return Err(Diagnostic::internal(
                    "field initializer needs its class receiver",
                ));
            };
            let instance = &self.class_insts[id];
            let info = &self.index.classes[instance.class];
            let fields = &self
                .hir
                .bodies
                .classes
                .get(info.hir_idx)
                .ok_or_else(|| Diagnostic::internal("missing class fields"))?
                .fields;
            let field_locs = instance.field_locs;
            for (f_idx, field) in fields.iter().enumerate() {
                let Some(init) = &field.init else { continue };
                let Some(&loc) = self.field_locs[field_locs].get(f_idx) else {
                    break;
                };
                if matches!(init, Expr::Array(items, _) if items.is_empty()) {
                    if self.types.is_open(self.locations[loc])
                        && !matches!(self.types.kind(self.locations[loc]), TyKind::Cell { .. })
                    {
                        let element = self.alloc_loc(Ty::BOTTOM);
                        let cell = self.types.cell(element, CellKind::Array);
                        self.write_loc(loc, cell);
                    }
                    if matches!(
                        self.types.kind(self.locations[loc]),
                        TyKind::Cell {
                            kind: CellKind::Array,
                            ..
                        }
                    ) {
                        continue;
                    }
                }
                let ty = self.eval_expr(&mut s, init)?;
                self.check_escaping_func(inst_id, ty, 0, s.current_span)?;
                let storage = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                self.write_loc(loc, storage);
                let dest = self.read_loc(loc, inst_id);
                self.record_coercion(inst_id, init, ty, dest);
            }
            return Ok(());
        }

        self.eval_stmts(&mut s, view.body)?;
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
                if crate::completion::returns_optional(&self.hir.declarations, view.body) {
                    self.types.with_absences(s.returned, Absence::Undefined)
                } else {
                    s.returned
                }
            } else if !crate::completion::falls_through(&self.hir.declarations, view.body) {
                Ty::NEVER
            } else {
                Ty::VOID
            };
            self.instances[inst_id].partial_record_result = (self.types.is_open(result_ty)
                && matches!(self.types.kind(result_ty), TyKind::Object { .. }))
            .then_some(result_ty);
            if view.is_async {
                while let Some(inner) = self.unwrap_promise_once(inst_id, result_ty) {
                    result_ty = inner;
                }
                if let TyKind::Cell {
                    loc: inner_loc,
                    kind: CellKind::Promise,
                } = self.types.kind(self.locations[return_loc])
                {
                    if result_ty != Ty::BOTTOM {
                        self.write_loc(inner_loc, result_ty);
                    }
                    return Ok(());
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
        let info = self.index.locals[local];
        let slot = info.frame_slot()?;
        (info.owner == self.instances[inst_id].template).then_some(slot)
    }

    pub(super) fn read_local_value(&mut self, inst_id: InstanceId, local: LocalIdx) -> Option<Ty> {
        if let Some(slot) = self.local_frame_slot_in(inst_id, local) {
            return self.inst_frame_locals[inst_id].get(slot).copied();
        }
        let loc = self.local_loc_in_instance(inst_id, local)?;
        Some(self.read_loc(loc, inst_id))
    }

    pub(super) fn write_local_value(&mut self, s: &mut EvalState<'h>, local: LocalIdx, value: Ty) {
        if let Some(cloc) = self.collapsed_local_locs[local] {
            self.write_loc(cloc, value);
        }
        if let Some(slot) = self.local_frame_slot_in(s.inst, local) {
            if let Some(cell) = self.inst_frame_locals[s.inst].get_mut(slot) {
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
        let info = self.index.locals[local];
        if info.flags.contains(LocalFlags::MODULE) {
            return info.module_loc();
        }
        let mut cursor = CaptureOwner::Instance(inst_id);
        loop {
            match cursor {
                CaptureOwner::Instance(cur) => {
                    if let Some(slot) = self.inst_locals[cur].iter().find(|s| s.local == local) {
                        return Some(slot.loc);
                    }
                    cursor = self.instances[cur].capture_owner;
                }
                CaptureOwner::Template(_) => return self.collapsed_local_locs[local],
                CaptureOwner::None => return None,
            }
        }
    }

    pub(super) fn ensure_instance_local(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
        initial: Ty,
    ) -> LocId {
        if let Some(loc) = self.index.locals[local].module_loc() {
            return loc;
        }
        if let Some(slot) = self.inst_locals[inst_id].iter().find(|s| s.local == local) {
            return slot.loc;
        }
        let loc = self.alloc_loc(initial);
        self.inst_locals[inst_id].push(InstanceLocalSlot { local, loc });
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

    pub(super) fn unwrap_promise_once(&mut self, inst_id: InstanceId, ty: Ty) -> Option<Ty> {
        match self.types.kind(ty) {
            TyKind::Promise(inner) => Some(inner),
            TyKind::Cell {
                loc,
                kind: CellKind::Promise,
            } => Some(self.read_loc(loc, inst_id)),
            _ => None,
        }
    }

    pub(super) fn constrain_promise_loc(&mut self, loc: LocId, expected: Ty) {
        if self.types.is_open(expected) {
            return;
        }
        if self.locations[loc] != expected {
            self.locations[loc] = expected;
            self.dirty_locs.insert(loc.index());
        }
        self.declared_locs.insert(loc.index());
    }

    pub(super) fn resolve_promise_cells(&mut self, inst_id: InstanceId, ty: Ty) -> Ty {
        match self.types.kind(ty) {
            TyKind::Cell {
                loc,
                kind: CellKind::Promise,
            } => {
                let elem = self.read_loc(loc, inst_id);
                if elem == Ty::BOTTOM {
                    Ty::BOTTOM
                } else {
                    self.types.promise(elem)
                }
            }
            TyKind::Cell { loc, kind } if kind.is_array() => {
                let elem = self.read_loc(loc, inst_id);
                if elem == Ty::BOTTOM {
                    Ty::BOTTOM
                } else {
                    let elem = self.resolve_promise_cells(inst_id, elem);
                    if elem == Ty::BOTTOM {
                        Ty::BOTTOM
                    } else {
                        self.types.array(elem)
                    }
                }
            }
            TyKind::Array(elem) => {
                let next = self.resolve_promise_cells(inst_id, elem);
                if next == Ty::BOTTOM {
                    Ty::BOTTOM
                } else if next == elem {
                    ty
                } else {
                    self.types.array(next)
                }
            }
            TyKind::Tuple(span) | TyKind::Union(span) => {
                let is_tuple = matches!(self.types.kind(ty), TyKind::Tuple(_));
                let items: Vec<Ty> = self.types.ty_slice(span).to_vec();
                let mut out = Vec::with_capacity(items.len());
                let mut changed = false;
                for item in items {
                    let next = self.resolve_promise_cells(inst_id, item);
                    if next == Ty::BOTTOM {
                        return Ty::BOTTOM;
                    }
                    changed |= next != item;
                    out.push(next);
                }
                if !changed {
                    ty
                } else if is_tuple {
                    self.types.tuple(&out)
                } else {
                    self.types.union(&out)
                }
            }
            _ => ty,
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
                if kind.is_promise() {
                    if elem == Ty::BOTTOM {
                        return Ty::BOTTOM;
                    }
                    let p = self.types.promise(elem);
                    return self.types.with_absences(p, abs);
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

    pub(super) fn propagate_writes(&mut self, caller: InstanceId, callee: InstanceId) {
        if let Ok([caller, callee]) = self
            .inst_written
            .as_mut_slice()
            .get_disjoint_mut([caller.index(), callee.index()])
        {
            caller.union_with(callee);
        }
    }

    pub(super) fn mark_written(&mut self, inst_id: InstanceId, local: LocalIdx) {
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            self.inst_written[cur].insert(local.index());
            cursor = self.instances[cur].capture_owner.instance();
        }
    }

    pub(super) fn record_site(&mut self, inst_id: InstanceId, expr: &Expr, res: SiteResolution) {
        let template = self.instances[inst_id].template;
        let Some(local_site) = self.index.site_of(template, expr) else {
            debug_assert!(false, "unindexed site in template {}", template.index());
            return;
        };
        if let Some(slot) = self.inst_sites[inst_id].get_mut(local_site) {
            slot.resolution = Some(res);
        }
    }

    pub(super) fn eval_stmts(&mut self, s: &mut EvalState<'h>, stmts: &'h [Stmt]) -> Result<()> {
        let depth = s.guards.depth();
        let mut remaining = stmts;
        while let Some((stmt, rest)) = remaining.split_first() {
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
                        && let Some(el) = s.iteration.element(remaining)
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
            remaining = rest;
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
                let local_flags = self.index.locals[lidx].flags;
                let is_inferred = self.index.is_inferred_local(self.hir, *id, lidx);
                let is_deferred = local_flags.contains(LocalFlags::DEFERRED);
                let declared = self.types.intern_hir(self.ctx, ty, !is_inferred);
                let declared = if !is_inferred {
                    self.instantiate_annotated_class(declared, 0, Some(s.inst))?
                } else {
                    declared
                };
                let needs_context = is_inferred || self.types.is_open(declared);
                let initial = if !needs_context { declared } else { Ty::BOTTOM };
                if self.local_frame_slot_in(s.inst, lidx).is_none() {
                    let _ = self.ensure_instance_local(s.inst, lidx, initial);
                }
                if !needs_context {
                    self.write_local_value(s, lidx, declared);
                }
                let is_i64_cast = is_inferred
                    && self
                        .types
                        .integer_domain(self.ctx, self.decls, declared)
                        .is_some_and(|domain| domain.deref(self.ctx).width() == 64);
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
                    } else if needs_context
                        && !local_flags.contains(LocalFlags::REASSIGNED)
                        && self.index.is_empty_object_expr(self.types, init_expr)
                    {
                        Some(CellKind::Dict)
                    } else {
                        None
                    };
                    if let Some(kind) = cell_kind {
                        let cell = self.alloc_expr_cell(s.inst, init_expr, kind)?;
                        self.write_local_value(s, lidx, cell);
                        self.mark_written(s.inst, lidx);
                        return Ok(());
                    }
                    let init_ty = if let Some(&class) = self.resource_registries.get(id) {
                        let value = self.instantiate_annotated_class(class, 0, Some(s.inst))?;
                        self.types.generic(
                            shapes::TypeName::new(self.ctx, "Map".into()),
                            &[Ty::NUMBER, value],
                        )
                    } else if matches!(init_expr, Expr::MapNew)
                        && matches!(self.types.kind(declared), TyKind::Generic { .. })
                        && !self.types.is_open(declared)
                    {
                        self.instantiate_annotated_class(declared, 0, Some(s.inst))?
                    } else {
                        self.eval_expr(s, init_expr)?
                    };
                    if !is_inferred
                        && let TyKind::Promise(expected_inner) = self.types.kind(declared)
                        && let TyKind::Cell {
                            loc,
                            kind: CellKind::Promise,
                        } = self.types.kind(init_ty)
                    {
                        self.constrain_promise_loc(loc, expected_inner);
                    }
                    if !matches!(init_expr, Expr::Undefined) {
                        self.mark_written(s.inst, lidx);
                    }
                    if matches!(self.types.kind(init_ty), TyKind::Func(_))
                        || self.is_tagged_func_union_in(init_ty, s.inst)
                    {
                        self.write_local_value(s, lidx, init_ty);
                    } else if !is_i64_cast && needs_context {
                        let is_kind = |t, k| matches!(self.types.kind(t), TyKind::Named(id) if crate::methods::Kind::builtin(id.get(self.ctx)) == Some(k));
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
                if matches!(value, Some(Expr::Array(elements, _)) if elements.is_empty())
                    && let Some(list) = self.instances[s.inst].list_result
                {
                    ty = list;
                }
                if let TyKind::Cell {
                    loc,
                    kind: CellKind::Dict,
                } = self.types.kind(ty)
                {
                    self.write_loc(loc, Ty::OPEN);
                    ty = self.types.object(&[], None);
                }
                if let Some(inst) = self.instances.get(s.inst)
                    && inst.has_explicit_return
                    && let TyKind::Promise(expected_inner) =
                        self.types.kind(self.locations[inst.return_loc])
                    && let TyKind::Cell {
                        loc,
                        kind: CellKind::Promise,
                    } = self.types.kind(ty)
                {
                    self.constrain_promise_loc(loc, expected_inner);
                }
                if self.index.view(self.instances[s.inst].template).is_async {
                    while let Some(inner) = self.unwrap_promise_once(s.inst, ty) {
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
                    crate::completion::falls_through(&self.hir.declarations, then_branch),
                    crate::completion::falls_through(&self.hir.declarations, otherwise),
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
                if let Some((id, source)) = self.hir.declarations.for_of(init.as_deref()) {
                    let iterable = self.eval_expr(s, source)?;
                    let iterable = self.resolve_cell_read(s.inst, iterable);
                    let element = if iterable == Ty::BOTTOM {
                        Ty::BOTTOM
                    } else {
                        builtins::iterated_element(self.ctx, self.types, iterable)
                            .unwrap_or(Ty::OPEN)
                    };
                    let local = self
                        .index
                        .local_idx(id)
                        .ok_or_else(|| Diagnostic::internal("missing iteration binding"))?;
                    self.write_local_value(s, local, element);
                    self.mark_written(s.inst, local);
                } else if let Some(init) = init {
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

    pub(super) fn finish(self, init_inst: InstanceId) -> Solved {
        let live_instances = self.compute_live_instances();

        let inst_local_tys = self.inst_locals.map(|slots| {
            slots
                .iter()
                .map(|sl| (self.index.locals[sl.local].id, self.locations[sl.loc]))
                .collect()
        });

        let mut class_static_field_tys = self.index.classes.map(|c| {
            vec![
                Ty::OPEN;
                self.hir
                    .bodies
                    .classes
                    .get(c.hir_idx)
                    .map_or(0, |hc| hc.static_fields.len())
            ]
        });
        for &(cid, f_idx, loc) in &self.static_field_locs {
            if let Some(slot) = class_static_field_tys[cid].get_mut(f_idx) {
                *slot = self.locations[loc];
            }
        }

        let mut class_inst_members = self.class_insts.filled(ClassInstMembers::default());
        let mut class_static_methods = self.index.classes.filled(Vec::new());
        for (id, inst) in self.instances.iter_enumerated() {
            if !live_instances.contains(id.index()) {
                continue;
            }
            let view = self.index.view(inst.template);
            match view.kind {
                TemplateKind::ClassMethod => {
                    if let Some(ci) = inst.receiver.and_then(Receiver::class) {
                        class_inst_members[ci].methods.push(id);
                    }
                }
                TemplateKind::ClassGetter(getter_idx) => {
                    if let Some(ci) = inst.receiver.and_then(Receiver::class) {
                        class_inst_members[ci].getters.push((getter_idx, id));
                    }
                }
                TemplateKind::ClassSetter(setter_idx) => {
                    if let Some(ci) = inst.receiver.and_then(Receiver::class) {
                        class_inst_members[ci].setters.push((setter_idx, id));
                    }
                }
                TemplateKind::ClassStaticMethod(class) => {
                    class_static_methods[class].push(id);
                }
                _ => {}
            }
        }

        Solved {
            witness_instances: self.witness_instances,
            init_inst,
            instances: self.instances,
            class_insts: self.class_insts,
            locations: self.locations,
            param_locs: self.param_locs,
            field_locs: self.field_locs,
            dispatch_arms: self.dispatch_arms,
            helpers: self.helpers,
            inst_local_tys,
            inst_frame_locals: self.inst_frame_locals,
            inst_sites: self.inst_sites,
            inst_captures: self.inst_captures,
            live_instances,
            escaping_env_templates: self.escaping_env_templates,
            collapsed_closures: self.collapsed_closures,
            boxed_locals: self.boxed_locals,
            collapsed_local_locs: self.collapsed_local_locs,
            class_static_field_tys,
            class_inst_members,
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

pub(super) fn first_type_var_name(ty: &HirType) -> Option<&str> {
    match ty {
        HirType::TypeVar(name) => Some(name.as_str()),
        HirType::Array(inner) | HirType::Promise(inner) => first_type_var_name(inner),
        HirType::Tuple(items) | HirType::Union(items) => items.iter().find_map(first_type_var_name),
        HirType::Generic { type_args, .. } => type_args.iter().find_map(first_type_var_name),
        _ => None,
    }
}
