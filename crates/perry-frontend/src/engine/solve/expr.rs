//! Expression, property, collection-constructor, and object-spread evaluation.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    fn eval_first(
        &mut self,
        s: &mut EvalState<'h>,
        first: &'h Expr,
        rest: &[&'h Expr],
    ) -> Result<Ty> {
        let ty = self.eval_expr(s, first)?;
        for &e in rest {
            self.eval_expr(s, e)?;
        }
        Ok(ty)
    }

    fn read_static_field(&mut self, inst_id: InstanceId, class_name: &str, field_name: &str) -> Ty {
        let (cn, fn_) = (
            self.types.intern_name(class_name),
            self.types.intern_name(field_name),
        );
        self.index
            .class_by_name_id(cn)
            .and_then(|cid| self.index.class(cid))
            .and_then(|c| ClassInfo::find(&c.static_field_locs, fn_))
            .map_or(Ty::OPEN, |loc| self.read_loc(loc, inst_id))
    }

    fn apply_resolved_method(
        &mut self,
        inst_id: InstanceId,
        recv: Ty,
        rule: crate::methods::MethodResult,
    ) -> Ty {
        let recv = self.resolve_cell_read(inst_id, recv);
        if recv == Ty::BOTTOM {
            Ty::BOTTOM
        } else {
            builtins::apply_method_result(
                self.ctx,
                self.types,
                rule,
                MethodResultArgs::receiver_only(recv),
            )
        }
    }

    pub(super) fn eval_expr(&mut self, s: &mut EvalState<'h>, expr: &'h Expr) -> Result<Ty> {
        if let Some(record) = s.iteration.expression(expr) {
            return match record {
                hir_compat::RecordExpr::Value(inner) => self.eval_expr(s, inner),
                hir_compat::RecordExpr::RequireObject(inner) => {
                    let ty = self.eval_expr(s, inner)?;
                    Ok(self.types.strip_nullish(ty).0)
                }
                hir_compat::RecordExpr::Length(src) => {
                    self.eval_expr(s, src)?;
                    Ok(Ty::NUMBER)
                }
                hir_compat::RecordExpr::Close => Ok(Ty::VOID),
            };
        }
        if self.metadata.constructors.get(expr).is_some() {
            return self.eval_collection_constructor(s, expr);
        }
        if let Some(cb_ty) = self.try_eval_builtin_callback(s, expr)? {
            return Ok(cb_ty);
        }

        match expr {
            Expr::LocalGet(id) => self.eval_local_get(s, *id),
            Expr::LocalSet(id, value) => {
                if s.iteration.is_release(expr) {
                    return Ok(Ty::VOID);
                }
                s.guards.assign(*id);
                let val_ty = self.eval_expr(s, value)?;
                if let Some(lidx) = self.index.local_idx(*id) {
                    if let Some(existing) = self.read_local_current(s.inst, lidx) {
                        if matches!(self.types.kind(existing), TyKind::Func(_)) {
                            return Err(Diagnostic::unsupported(
                                "a reassigned function binding requires a runtime function representation",
                            ));
                        }
                        let is_inferred = self.index.is_inferred_local(self.hir, *id, lidx);
                        if let TyKind::Cell { loc, kind } = self.types.kind(existing) {
                            self.write_loc(loc, Ty::OPEN);
                            if kind == CellKind::Dict && is_inferred && val_ty != Ty::BOTTOM {
                                let norm = self
                                    .types
                                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                                self.set_local_fixed(s, lidx, norm);
                            }
                        } else if (is_inferred
                            && self.index.local_deferred.contains(lidx.0 as usize))
                            || self.types.is_open(existing)
                            || (is_inferred && matches!(existing, Ty::NULL | Ty::VOID))
                        {
                            let norm = self
                                .types
                                .normalize_for_storage(self.ctx, self.decls, val_ty);
                            self.write_local_value(s, lidx, norm);
                        }
                    }
                    self.mark_written(s.inst, lidx);
                }
                Ok(val_ty)
            }
            Expr::This => Ok(s.receiver.map_or(Ty::OPEN, |c| self.types.class_inst(c))),
            Expr::FuncRef(fid) => {
                Ok(self
                    .index
                    .template_of_func(*fid)
                    .map_or(Ty::OPEN, |template| {
                        self.types.func(FuncIdentity {
                            template,
                            capture_owner: None,
                        })
                    }))
            }
            Expr::Closure {
                func_id,
                captures,
                body,
                ..
            } => {
                let mut refs = captures.clone();
                let mut defined = std::collections::HashSet::new();
                let stmts = if !body.is_empty() {
                    body.as_slice()
                } else {
                    self.index
                        .template_of_func(*func_id)
                        .and_then(|t| self.index.view(t))
                        .map_or(&[][..], |v| v.body)
                };
                for stmt in stmts {
                    perry_hir::analysis::collect_local_refs_stmt(stmt, &mut refs, &mut defined);
                }
                for lid in refs {
                    if let Some(ty) = self
                        .index
                        .local_idx(lid)
                        .and_then(|l| self.read_local_current(s.inst, l))
                    {
                        self.invalidate_conservative_cell(ty);
                    }
                }
                let Some(template) = self.index.template_of_func(*func_id) else {
                    return Ok(Ty::OPEN);
                };
                Ok(self.types.func(FuncIdentity {
                    template,
                    capture_owner: self.index.captures_local(captures).then_some(s.inst),
                }))
            }
            Expr::Binary { op, left, right } => {
                let (l, r) = (self.eval_expr(s, left)?, self.eval_expr(s, right)?);
                Ok(builtins::binary_result(self.ctx, self.types, *op, l, r))
            }
            Expr::Unary { op, operand } => {
                Ok(builtins::unary_result(*op, self.eval_expr(s, operand)?))
            }
            Expr::Logical { op, left, right } => {
                let l = self.eval_expr(s, left)?;
                let depth = s.guards.depth();
                if matches!(op, LogicalOp::And | LogicalOp::Or) {
                    s.guards.exclude_absences(left, *op == LogicalOp::And);
                }
                let r = self.eval_expr(s, right)?;
                s.guards.truncate(depth);
                Ok(builtins::logical_result(self.types, *op, l, r))
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => {
                self.eval_expr(s, condition)?;
                let guard = DefinedGuard::of(condition);
                let branch = s.guards.branch();
                s.guards.exclude_absences(condition, true);
                let mut then_ty = self.eval_expr(s, then_expr)?;
                if guard.as_ref().is_some_and(|g| g.proves(true, then_expr)) {
                    then_ty = self.types.exclude_absences(then_ty, Absence::Undefined);
                }
                let then_writes = s.guards.other_arm(branch);
                s.guards.exclude_absences(condition, false);
                let mut else_ty = self.eval_expr(s, else_expr)?;
                if guard.as_ref().is_some_and(|g| g.proves(false, else_expr)) {
                    else_ty = self.types.exclude_absences(else_ty, Absence::Undefined);
                }
                s.guards.join(branch, then_writes, true, true);
                Ok(self.types.join(then_ty, else_ty))
            }
            Expr::Sequence(parts) => {
                let mut last = Ty::VOID;
                for part in parts {
                    last = self.eval_expr(s, part)?;
                }
                Ok(last)
            }
            Expr::ScopedTemp { id, value, body } => {
                let val_ty = self.eval_expr(s, value)?;
                if let Some(lidx) = self.index.local_idx(*id) {
                    self.write_local_value(s, lidx, val_ty);
                    self.mark_written(s.inst, lidx);
                }
                self.eval_expr(s, body)
            }
            Expr::Await(promise) => {
                let ty = self.eval_expr(s, promise)?;
                Ok(match self.types.kind(ty) {
                    TyKind::Promise(i) => i,
                    _ => ty,
                })
            }
            Expr::StructuredClone { value, .. } => self.eval_expr(s, value),
            Expr::Array(elements, _) => {
                if elements.is_empty() {
                    return Ok(Ty::OPEN);
                }
                let elem_tys = self.eval_args(s, elements)?;
                if elem_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                if let [a, b] = elem_tys.as_slice()
                    && self.types.strip_nullish(*a).0 != self.types.strip_nullish(*b).0
                    && !self.types.is_open(*a)
                    && !self.types.is_open(*b)
                {
                    return Ok(self.types.tuple(&[*a, *b]));
                }
                let joined = elem_tys
                    .into_iter()
                    .fold(Ty::BOTTOM, |acc, t| self.types.join(acc, t));
                Ok(self.types.array(joined))
            }
            Expr::ArraySpread(items) => {
                let (mut joined, mut has_bottom) = (Ty::BOTTOM, false);
                for item in items {
                    let t = match item {
                        perry_hir::ir::ArrayElement::Expr(e) => {
                            let v = self.eval_expr(s, e)?;
                            has_bottom |= v == Ty::BOTTOM;
                            v
                        }
                        perry_hir::ir::ArrayElement::Hole => Ty::VOID,
                        perry_hir::ir::ArrayElement::Spread(Expr::Array(elems, _))
                            if elems.is_empty() =>
                        {
                            Ty::NEVER
                        }
                        perry_hir::ir::ArrayElement::Spread(e) => {
                            let src = self.eval_expr(s, e)?;
                            let src = self.resolve_cell_read(s.inst, src);
                            if src == Ty::BOTTOM {
                                has_bottom = true;
                                Ty::BOTTOM
                            } else {
                                builtins::iterated_element(self.ctx, self.types, src)
                                    .unwrap_or(Ty::OPEN)
                            }
                        }
                    };
                    joined = self.types.join(joined, t);
                }
                if has_bottom {
                    return Ok(Ty::BOTTOM);
                }
                let (inner, abs) = self.types.strip_nullish(joined);
                if inner == Ty::VOID && abs.is_some_and(Absence::admits_null) {
                    joined = self.types.with_absences(Ty::NUMBER, abs);
                }
                Ok(self.types.array(joined))
            }
            Expr::ArrayFrom(source, _) => {
                let src = self.eval_expr(s, source)?;
                let src = self.resolve_cell_read(s.inst, src);
                if src == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                if matches!(source.as_ref(), Expr::Object(p) if p.iter().any(|(k, _)| k == "length"))
                {
                    return Ok(self.types.array(Ty::OPEN));
                }
                Ok(builtins::iterated_element(self.ctx, self.types, src)
                    .map_or(Ty::OPEN, |e| self.types.array(e)))
            }
            Expr::Object(fields) => {
                self.eval_object_fields(s, fields.iter().map(|(n, v)| (n.as_str(), v)))
            }
            Expr::ObjectSpread { parts } => self.eval_object_spread(s, parts),
            Expr::ObjectRest {
                object,
                exclude_keys,
            } => {
                let src = self.eval_expr(s, object)?;
                if src == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(src);
                if stripped == Ty::DYNAMIC {
                    return Ok(Ty::DYNAMIC);
                }
                if let Some(src_props) = self.object_props_of(stripped) {
                    let kept: Vec<_> = src_props
                        .into_iter()
                        .filter(|p| {
                            !exclude_keys
                                .iter()
                                .any(|k| k == self.types.name_str(p.name))
                        })
                        .collect();
                    return Ok(self.types.object(&kept, None));
                }
                Ok(Ty::OPEN)
            }
            Expr::PropertyGet {
                object, property, ..
            } => self.eval_property_get(s, object, property),
            Expr::PropertySet {
                object,
                property,
                value,
                ..
            } => {
                let (obj_ty, val_ty) = (self.eval_expr(s, object)?, self.eval_expr(s, value)?);
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                self.write_class_or_setter(s.inst, obj_def, property, value, val_ty)?;
                if let TyKind::Cell { loc, kind } = self.types.kind(obj_def) {
                    if kind == CellKind::Dict {
                        self.write_cell(loc, val_ty);
                    } else {
                        self.write_loc(loc, Ty::OPEN);
                    }
                }
                Ok(val_ty)
            }
            Expr::IndexGet { object, index, .. } => {
                let (obj_ty, idx_ty) = (self.eval_expr(s, object)?, self.eval_expr(s, index)?);
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                if let TyKind::Cell { loc, kind } = self.types.kind(obj_def) {
                    if kind == CellKind::Dict && self.dict_reads.insert(loc.0 as usize) {
                        self.dirty_locs.insert(loc.0 as usize);
                    }
                    if idx_ty != Ty::BOTTOM
                        && !(if kind.is_array() {
                            matches!(idx_ty, Ty::NUMBER | Ty::INT32)
                        } else {
                            idx_ty == Ty::STRING
                        })
                    {
                        self.write_loc(loc, Ty::OPEN);
                    }
                }
                let obj_ty = self.resolve_cell_read(s.inst, obj_ty);
                if obj_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(
                    builtins::index_type(self.ctx, self.decls, self.types, obj_ty, index)
                        .unwrap_or(Ty::OPEN),
                )
            }
            Expr::IndexSet {
                object,
                index,
                value,
                ..
            }
            | Expr::PutValueSet {
                target: object,
                key: index,
                value,
                ..
            } => {
                let (obj_ty, idx_ty, val_ty) = (
                    self.eval_expr(s, object)?,
                    self.eval_expr(s, index)?,
                    self.eval_expr(s, value)?,
                );
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                if matches!(expr, Expr::PutValueSet { .. })
                    && let Expr::String(field_name) = index.as_ref()
                {
                    self.write_class_or_setter(s.inst, obj_def, field_name, value, val_ty)?;
                }
                self.write_cell_indexed(obj_def, idx_ty, val_ty);
                Ok(val_ty)
            }
            Expr::StaticFieldGet {
                class_name,
                field_name,
            } => Ok(self.read_static_field(s.inst, class_name, field_name)),
            Expr::StaticFieldSet {
                class_name,
                field_name,
                value,
            } => {
                let val_ty = self.eval_expr(s, value)?;
                let (cn, fn_) = (
                    self.types.intern_name(class_name),
                    self.types.intern_name(field_name),
                );
                if let Some(loc) = self
                    .index
                    .class_by_name_id(cn)
                    .and_then(|cid| self.index.class(cid))
                    .and_then(|c| ClassInfo::find(&c.static_field_locs, fn_))
                {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, val_ty);
                    self.write_loc(loc, norm);
                }
                Ok(val_ty)
            }
            Expr::StaticMethodCall {
                class_name,
                method_name,
                args,
                ..
            } => self.eval_static_method_call(s, expr, class_name, method_name, args, 0),
            Expr::New {
                class_name,
                args,
                type_args,
                byte_offset,
                ..
            } => self.eval_new(s, expr, class_name, args, type_args, *byte_offset),
            Expr::Call {
                callee,
                args,
                type_args,
                byte_offset,
                ..
            } => self.eval_call(s, expr, callee, args, type_args, *byte_offset),
            Expr::CallSpread { callee, args, .. } => self.eval_call_spread(s, expr, callee, args),
            Expr::NativeMethodCall {
                module,
                class_name,
                object,
                method,
                args,
                ..
            } => {
                if matches!(module.as_str(), "stream" | "node:stream")
                    && class_name.as_deref() == Some("Writable")
                    && object.is_none()
                    && method == "toWeb"
                {
                    self.eval_args(s, args)?;
                    return Ok(self.types.named(self.ctx, "WritableStream"));
                }
                if module == "__perry_runtime" && method == "requireObjectCoercible" {
                    return match args.first() {
                        Some(a) => {
                            let ty = self.eval_expr(s, a)?;
                            Ok(self.types.strip_nullish(ty).0)
                        }
                        None => Ok(Ty::OPEN),
                    };
                }
                let arg_tys = self.eval_args(s, args)?;
                if let Some(obj) = object {
                    let obj_ty = self.eval_expr(s, obj)?;
                    if obj_ty == Ty::BOTTOM {
                        return Ok(Ty::BOTTOM);
                    }
                    let (stripped, _) = self.types.strip_nullish(obj_ty);
                    let rule = args
                        .is_empty()
                        .then(|| {
                            builtins::lookup_method_result(
                                self.ctx, self.types, stripped, method, true,
                            )
                        })
                        .flatten()
                        .or_else(|| {
                            builtins::lookup_method_result(
                                self.ctx, self.types, stripped, method, false,
                            )
                        });
                    if let Some(rule) = rule {
                        return Ok(builtins::apply_method_result(
                            self.ctx,
                            self.types,
                            rule,
                            MethodResultArgs {
                                receiver: obj_ty,
                                arg0: arg_tys.first().copied(),
                                arg0_expr: args.first(),
                                callback_result: None,
                                sparse_arrays: self.facts.sparse_arrays,
                            },
                        ));
                    }
                }
                Ok(Ty::OPEN)
            }
            Expr::ObjectValues(object) | Expr::ObjectEntries(object) => {
                let obj_ty = self.eval_expr(s, object)?;
                if obj_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let obj_ty = self.resolve_cell_read(s.inst, obj_ty);
                let stripped = self.types.strip_nullish(obj_ty).0;
                let hir = self.types.materialize_hir(self.ctx, stripped, &|_| None);
                Ok(self
                    .decls
                    .dict_entry_types(self.ctx, &hir)
                    .map_or(Ty::OPEN, |(k, v)| {
                        let v = self.types.intern_hir(self.ctx, &v, true);
                        let elem = if matches!(expr, Expr::ObjectEntries(_)) {
                            let k = self.types.intern_hir(self.ctx, &k, true);
                            self.types.tuple(&[k, v])
                        } else {
                            v
                        };
                        self.types.array(elem)
                    }))
            }
            Expr::ObjectFromEntries(entries) => {
                let ent_ty = self.eval_expr(s, entries)?;
                if ent_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let stripped = self.types.strip_nullish(ent_ty).0;
                if let TyKind::Array(inner) = self.types.kind(stripped)
                    && let TyKind::Tuple(span) = self.types.kind(inner)
                    && let [_, val] = self.types.ty_slice(span)
                {
                    let val = *val;
                    return Ok(self.types.generic(
                        shapes::TypeId::from_owned(self.ctx, HirType::Named("Record".into())),
                        &[Ty::STRING, val],
                    ));
                }
                Ok(Ty::OPEN)
            }
            Expr::ArraySlice {
                array, start, end, ..
            } => {
                let ty = self.eval_first(s, array, &[start])?;
                if let Some(e) = end {
                    self.eval_expr(s, e)?;
                }
                Ok(ty)
            }
            Expr::ArrayToReversed { array }
            | Expr::ArraySort { array, .. }
            | Expr::ArrayToSorted { array, .. } => self.eval_expr(s, array),
            Expr::ArrayWith {
                array,
                index,
                value,
            } => self.eval_first(s, array, &[index, value]),
            Expr::MapSet { map, key, value } => self.eval_first(s, map, &[key, value]),
            Expr::CryptoRandomFillSync {
                buffer,
                offset,
                size,
            } => self.eval_first(s, buffer, &[offset, size]),
            Expr::ArrayPop(id) | Expr::ArrayShift(id) => {
                let arr = self.eval_local_get(s, *id)?;
                Ok(self.apply_resolved_method(s.inst, arr, crate::methods::MethodResult::Element))
            }
            Expr::ArrayAt { array, index } => {
                let arr = self.eval_first(s, array, &[index])?;
                Ok(self.apply_resolved_method(s.inst, arr, crate::methods::MethodResult::At))
            }
            Expr::MapGet { map, key } => {
                let m = self.eval_first(s, map, &[key])?;
                Ok(self.apply_resolved_method(s.inst, m, crate::methods::MethodResult::Lookup))
            }
            Expr::ArrayKeys(src)
            | Expr::MapKeys(src)
            | Expr::ArrayValues(src)
            | Expr::MapValues(src)
            | Expr::SetValues(src)
            | Expr::ArrayEntries(src)
            | Expr::MapEntries(src) => {
                let src_ty = self.eval_expr(s, src)?;
                let src_ty = self.resolve_cell_read(s.inst, src_ty);
                if src_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let m = match expr {
                    Expr::ArrayKeys(_) | Expr::MapKeys(_) => {
                        crate::collections::IteratorMethod::Keys
                    }
                    Expr::ArrayValues(_) | Expr::MapValues(_) | Expr::SetValues(_) => {
                        crate::collections::IteratorMethod::Values
                    }
                    _ => crate::collections::IteratorMethod::Entries,
                };
                Ok(builtins::iterator_view(self.ctx, self.types, src_ty, m).unwrap_or(Ty::OPEN))
            }
            Expr::SetAdd { set_id, value } => {
                self.eval_expr(s, value)?;
                self.eval_local_get(s, *set_id)
            }
            Expr::Compare { op, left, right }
                if matches!(
                    op,
                    CompareOp::Eq | CompareOp::Ne | CompareOp::LooseEq | CompareOp::LooseNe
                ) =>
            {
                let (l, r) = (self.eval_expr(s, left)?, self.eval_expr(s, right)?);
                if let (TyKind::Func(a), TyKind::Func(b)) = (self.types.kind(l), self.types.kind(r))
                {
                    self.fold_func_compare(s.inst, expr, *op, a, b, left, right)?;
                }
                Ok(Ty::BOOLEAN)
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => {
                let val_ty = self.eval_expr(s, value)?;
                if let Some(arr_ty) = self
                    .index
                    .local_idx(*array_id)
                    .and_then(|l| self.read_local_value(s.inst, l))
                {
                    let def = self.types.strip_nullish(arr_ty).0;
                    if let TyKind::Cell { loc, kind } = self.types.kind(def) {
                        if kind.is_array() {
                            self.write_cell(loc, val_ty);
                        } else {
                            self.write_loc(loc, Ty::OPEN);
                        }
                    }
                }
                Ok(Ty::NUMBER)
            }
            Expr::Delete(target) => {
                if let Expr::IndexGet { object, .. } | Expr::PropertyGet { object, .. } =
                    target.as_ref()
                {
                    let obj_ty = self.eval_expr(s, object)?;
                    if self.type_may_be_array(obj_ty) {
                        self.deletes_array_elements = true;
                    }
                } else {
                    self.eval_children(s, expr)?;
                }
                Ok(Ty::BOOLEAN)
            }
            Expr::Update { id, .. } => {
                s.guards.assign(*id);
                if let Some(lidx) = self.index.local_idx(*id) {
                    self.mark_written(s.inst, lidx);
                }
                Ok(Ty::NUMBER)
            }
            _ => {
                self.eval_children(s, expr)?;
                Ok(builtins::leaf_expr_type(self.ctx, self.types, expr))
            }
        }
    }

    fn eval_children(&mut self, s: &mut EvalState<'h>, expr: &'h Expr) -> Result<()> {
        let mut err = None;
        perry_hir::walker::walk_expr_children(expr, &mut |child| {
            if err.is_none()
                && let Err(e) = self.eval_expr(s, child)
            {
                err = Some(e);
            }
        });
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fold_func_compare(
        &mut self,
        inst_id: InstanceId,
        expr: &'h Expr,
        op: CompareOp,
        a: FuncIdentity,
        b: FuncIdentity,
        left: &Expr,
        right: &Expr,
    ) -> Result<()> {
        for (ident, side) in [(a, left), (b, right)] {
            if ident.capture_owner.is_some()
                || self
                    .index
                    .view(ident.template)
                    .is_some_and(|v| self.index.captures_local(v.captures))
            {
                let raw = self.index.view(ident.template).map_or("", |v| v.name);
                let name = if raw.is_empty()
                    && let Expr::LocalGet(lid) = side
                    && let Some(src) = self
                        .hir
                        .local_source_spans
                        .get(lid)
                        .and_then(|sp| self.metadata.locations.source_at(sp.start))
                {
                    src.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
                        .next()
                        .unwrap_or("closure")
                } else if raw.is_empty() {
                    "closure"
                } else {
                    raw
                };
                let err =
                    Diagnostic::unsupported(format!("cannot compare capturing function '{name}'"));
                return Err(match self.facts.callable_comparisons.first() {
                    Some(&span) => self.metadata.locations.annotate(err, span),
                    None => err,
                });
            }
        }
        self.record_site(
            inst_id,
            expr,
            SiteResolution::FoldBool(
                (a.template == b.template) == matches!(op, CompareOp::Eq | CompareOp::LooseEq),
            ),
        );
        Ok(())
    }

    fn write_class_or_setter(
        &mut self,
        inst_id: InstanceId,
        obj_def: Ty,
        property: &str,
        value: &Expr,
        val_ty: Ty,
    ) -> Result<()> {
        let TyKind::ClassInst(cinst_id) = self.types.kind(obj_def) else {
            return Ok(());
        };
        let norm = self
            .types
            .normalize_for_storage(self.ctx, self.decls, val_ty);
        if let Some(loc) = self.class_field_loc(cinst_id, property) {
            let existing = self.locations[loc.0 as usize];
            if matches!(value, Expr::Array(items, _) if items.is_empty()) {
                if !matches!(
                    self.types.kind(existing),
                    TyKind::Cell {
                        kind: CellKind::Array,
                        ..
                    }
                ) && self.types.is_open(existing)
                {
                    let elem_loc = self.alloc_loc(Ty::BOTTOM);
                    let cell = self.types.array_cell(elem_loc);
                    self.write_loc(loc, cell);
                }
            } else if !self.types.is_open(norm) || self.types.is_open(existing) {
                self.write_loc(loc, norm);
            }
        }
        let prop_id = self.types.intern_name(property);
        if norm != Ty::BOTTOM
            && let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
            && let Some(setter_tid) = self
                .index
                .class(cinst.class)
                .and_then(|c| ClassInfo::find(&c.setters, prop_id))
        {
            self.request_instance(
                InstanceReq::new(setter_tid, &[norm], Some(inst_id)).receiver(Some(cinst_id)),
            )?;
        }
        Ok(())
    }

    pub(super) fn eval_local_get(&mut self, s: &mut EvalState<'h>, id: LocalId) -> Result<Ty> {
        let Some(lidx) = self.index.local_idx(id) else {
            return Ok(Ty::OPEN);
        };
        let Some(mut ty) = self.read_local_value(s.inst, lidx) else {
            return Ok(self
                .index
                .local_declared
                .get(lidx.0 as usize)
                .copied()
                .unwrap_or(Ty::OPEN));
        };
        if self.index.local_deferred.contains(lidx.0 as usize)
            && self
                .inst_written
                .get(s.inst.0 as usize)
                .is_none_or(|w| !w.contains(lidx.0 as usize))
            && ty == Ty::BOTTOM
        {
            return Ok(Ty::VOID);
        }
        if !self
            .index
            .local_written_from_closure
            .contains(lidx.0 as usize)
            && let Some(ex) = s.guards.excluded(id)
        {
            ty = self.types.exclude_absences(ty, ex);
        }
        Ok(ty)
    }

    pub(super) fn eval_collection_constructor(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
    ) -> Result<Ty> {
        let (is_map, explicit_tys, init) = match self.metadata.constructors.get(expr) {
            Some(hir_compat::Constructor::Map { types, init }) => (
                true,
                types.map(|(k, v)| {
                    vec![
                        self.types.intern_hir(self.ctx, k, true),
                        self.types.intern_hir(self.ctx, v, true),
                    ]
                }),
                init,
            ),
            Some(hir_compat::Constructor::Set { element, init }) => (
                false,
                element.map(|e| vec![self.types.intern_hir(self.ctx, e, true)]),
                init,
            ),
            None => return Ok(Ty::OPEN),
        };
        if let Some(init) = init {
            self.eval_expr(s, init)?;
        }
        let base = shapes::TypeId::from_owned(
            self.ctx,
            HirType::Named(if is_map { "Map" } else { "Set" }.into()),
        );
        if let Some(mut arg_tys) = explicit_tys {
            let bindings = self.active_instance_type_bindings(s.inst);
            for ty in &mut arg_tys {
                *ty = self.types.substitute(self.ctx, *ty, &bindings);
            }
            if !bindings.is_empty() {
                self.record_site(
                    s.inst,
                    expr,
                    SiteResolution::CollectionNew {
                        is_map,
                        type_args: arg_tys.clone(),
                    },
                );
            }
            return Ok(self.types.generic(base, &arg_tys));
        }
        if let Some(init) = init {
            if is_map {
                if let Expr::Array(entries, _) = init
                    && let Some(Expr::Array(pair, _)) = entries.first()
                    && let [key, value] = pair.as_slice()
                {
                    let (k, v) = (self.eval_expr(s, key)?, self.eval_expr(s, value)?);
                    return Ok(if k == Ty::BOTTOM || v == Ty::BOTTOM {
                        Ty::BOTTOM
                    } else {
                        self.types.generic(base, &[k, v])
                    });
                }
                let init_ty = self.eval_expr(s, init)?;
                if init_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let stripped = self.types.strip_nullish(init_ty).0;
                if let TyKind::Array(elem) = self.types.kind(stripped)
                    && let TyKind::Tuple(span) = self.types.kind(elem)
                    && let [k, v] = self.types.ty_slice(span)
                {
                    let (k, v) = (*k, *v);
                    return Ok(self.types.generic(base, &[k, v]));
                }
            } else {
                let init_ty = self.eval_expr(s, init)?;
                if init_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                if let Some(elem) = builtins::iterated_element(self.ctx, self.types, init_ty) {
                    return Ok(self.types.generic(base, &[elem]));
                }
            }
        }
        Ok(Ty::OPEN)
    }

    pub(super) fn active_instance_type_bindings(
        &mut self,
        inst_id: InstanceId,
    ) -> Vec<(shapes::TypeId, Ty)> {
        let Some(inst) = self.instances.get(inst_id.0 as usize) else {
            return Vec::new();
        };
        let (template, receiver, capture_owner) =
            (inst.template, inst.receiver, inst.capture_owner);
        let args = self
            .instance_args
            .get(inst.args.range())
            .unwrap_or(&[])
            .to_vec();
        let explicit_targs = self
            .instance_type_args
            .get(inst.explicit_type_args.range())
            .unwrap_or(&[])
            .to_vec();
        self.compute_type_param_bindings(template, receiver, capture_owner, &args, &explicit_targs)
    }

    pub(super) fn eval_property_get(
        &mut self,
        s: &mut EvalState<'h>,
        object: &'h Expr,
        property: &str,
    ) -> Result<Ty> {
        if let Expr::ClassRef(class_name) = object {
            return Ok(self.read_static_field(s.inst, class_name, property));
        }
        let mut obj_ty = self.eval_expr(s, object)?;
        if obj_ty == Ty::BOTTOM {
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        if let TyKind::Cell {
            loc,
            kind: CellKind::Dict,
        } = self.types.kind(stripped)
        {
            if self.dict_reads.insert(loc.0 as usize) {
                self.dirty_locs.insert(loc.0 as usize);
            }
            obj_ty = self.resolve_cell_read(s.inst, obj_ty);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        if let Some(cinst_id) = self.resolve_class_inst(s.inst, stripped, 0)? {
            if let Some(loc) = self.class_field_loc(cinst_id, property) {
                return Ok(self.read_loc(loc, s.inst));
            }
            let pname = self.types.intern_name(property);
            if let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
                && let Some(getter_tid) = self
                    .index
                    .class(cinst.class)
                    .and_then(|c| ClassInfo::find(&c.getters, pname))
            {
                let target = self.request_instance(
                    InstanceReq::new(getter_tid, &[], Some(s.inst)).receiver(Some(cinst_id)),
                )?;
                return Ok(self.read_loc(self.instances[target.0 as usize].return_loc, s.inst));
            }
        }
        Ok(
            builtins::property_type(self.ctx, self.decls, self.types, obj_ty, property)
                .unwrap_or(Ty::OPEN),
        )
    }

    fn class_field_loc(&self, cinst_id: ClassInstId, property: &str) -> Option<LocId> {
        let cinst = self.class_insts.get(cinst_id.0 as usize)?;
        let f_idx = self
            .hir
            .classes
            .get(self.index.class(cinst.class)?.hir_idx)?
            .fields
            .iter()
            .position(|f| f.name == property)?;
        Some(LocId(cinst.field_loc_base + f_idx as u32))
    }

    fn object_props_of(&mut self, stripped: Ty) -> Option<Vec<ObjectProp>> {
        if let TyKind::Object { props, .. } = self.types.kind(stripped) {
            return Some(self.types.object_slice(props).to_vec());
        }
        if matches!(
            self.types.kind(stripped),
            TyKind::Leaf(_) | TyKind::Generic { .. }
        ) {
            let hir = self.types.materialize_hir(self.ctx, stripped, &|_| None);
            return Some(
                self.decls
                    .object_properties(self.ctx, &hir)?
                    .into_iter()
                    .map(|(n, fh, optional)| {
                        let name = self.types.intern_name(&n);
                        let mut ty = self.types.intern_hir(self.ctx, &fh, true);
                        if optional {
                            ty = self.types.with_absences(ty, Absence::Undefined);
                        }
                        ObjectProp {
                            name,
                            ty,
                            optional,
                            readonly: false,
                        }
                    })
                    .collect(),
            );
        }
        None
    }

    pub(super) fn eval_object_spread(
        &mut self,
        s: &mut EvalState<'h>,
        parts: &'h [perry_hir::ir::ObjectPart],
    ) -> Result<Ty> {
        let mut props: Vec<ObjectProp> = Vec::new();
        let (mut is_dynamic, mut has_bottom) = (false, false);
        for part in parts {
            let val_expr = part.value();
            let val_ty = self.eval_expr(s, val_expr)?;
            if val_ty == Ty::BOTTOM {
                has_bottom = true;
                continue;
            }
            if let Some(key) = hir_compat::static_property(part) {
                if let TyKind::Func(ident) = self.types.kind(val_ty) {
                    self.check_escaping_unannotated_func(ident.template)?;
                }
                let name = self.types.intern_name(key);
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                if let Some(ex) = props.iter_mut().find(|p| p.name == name) {
                    ex.ty = match val_expr {
                        Expr::Null => self.types.with_absences(ex.ty, Absence::Null),
                        Expr::Undefined => self.types.with_absences(ex.ty, Absence::Undefined),
                        _ => norm,
                    };
                } else {
                    props.push(ObjectProp {
                        name,
                        ty: norm,
                        optional: false,
                        readonly: false,
                    });
                }
            } else if part.defines_key() {
                return Ok(Ty::OPEN);
            } else {
                let (stripped, _) = self.types.strip_nullish(val_ty);
                if stripped == Ty::DYNAMIC {
                    is_dynamic = true;
                    continue;
                }
                for sp in self.object_props_of(stripped).into_iter().flatten() {
                    if let Some(ex) = props.iter_mut().find(|p| p.name == sp.name) {
                        *ex = sp;
                    } else {
                        props.push(sp);
                    }
                }
            }
        }
        if has_bottom {
            Ok(Ty::BOTTOM)
        } else if is_dynamic {
            Ok(Ty::DYNAMIC)
        } else {
            Ok(self.types.object(&props, None))
        }
    }
}
