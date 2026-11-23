//! Expression, property, collection-constructor, and object-spread evaluation.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn eval_expr(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // 0. Compiler-owned dense-array iterator records (skip dead iterator-protocol branch).
        if let Some(record) = state.iteration.expression(expr) {
            return match record {
                hir_compat::RecordExpr::Value(inner) => {
                    self.eval_expr(inst_id, receiver, inner, state)
                }
                hir_compat::RecordExpr::RequireObject(inner) => {
                    let ty = self.eval_expr(inst_id, receiver, inner, state)?;
                    Ok(self.types.strip_nullish(ty).0)
                }
                hir_compat::RecordExpr::Length(src) => {
                    self.eval_expr(inst_id, receiver, src, state)?;
                    Ok(Ty::NUMBER)
                }
                hir_compat::RecordExpr::Close => Ok(Ty::VOID),
            };
        }

        // 1. Preserved `new Map` / `new Set` constructors.
        if self.metadata.constructors.get(expr).is_some() {
            return self.eval_collection_constructor(inst_id, receiver, expr, state);
        }

        // 2. Built-in callbacks (`map`, `forEach`, `reduce`, `sort`, `replace`, `Array.from`, `Map.groupBy`).
        if let Some(cb_ty) = self.try_eval_builtin_callback(inst_id, receiver, expr, state)? {
            return Ok(cb_ty);
        }

        match expr {
            Expr::LocalGet(id) => self.eval_local_get(inst_id, *id, &state.guards),
            Expr::LocalSet(id, value) => {
                if state.iteration.is_release(expr) {
                    return Ok(Ty::VOID);
                }
                state.guards.assign(*id);
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Some(lidx) = self.index.local_idx(*id) {
                    if let Some(existing) = self.read_local_current(inst_id, lidx) {
                        if matches!(self.types.kind(existing), TyKind::Func(_)) {
                            return Err(Diagnostic::unsupported(
                                "a reassigned function binding requires a runtime function representation",
                            ));
                        }
                        let is_inferred = self.index.local_inferred.contains(lidx.0 as usize)
                            || !self.hir.local_source_spans.contains_key(id);
                        if let TyKind::Cell { loc, kind } = self.types.kind(existing) {
                            self.write_loc(loc, Ty::OPEN);
                            if kind == CellKind::Dict && is_inferred && val_ty != Ty::BOTTOM {
                                let norm = self
                                    .types
                                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                                self.set_local_fixed(inst_id, lidx, norm, state);
                            }
                        } else if (is_inferred
                            && self.index.local_deferred.contains(lidx.0 as usize))
                            || self.types.is_open(existing)
                            || (is_inferred && matches!(existing, Ty::NULL | Ty::VOID))
                        {
                            let norm = self
                                .types
                                .normalize_for_storage(self.ctx, self.decls, val_ty);
                            self.write_local_value(inst_id, lidx, norm, state);
                        }
                    }
                    self.mark_written(inst_id, lidx);
                }
                Ok(val_ty)
            }
            Expr::This => Ok(receiver.map_or(Ty::OPEN, |cinst| self.types.class_inst(cinst))),
            Expr::FuncRef(fid) => {
                if let Some(tid) = self.index.template_of_func(*fid) {
                    Ok(self.types.func(FuncIdentity {
                        template: tid,
                        capture_owner: None,
                    }))
                } else {
                    Ok(Ty::OPEN)
                }
            }
            Expr::Closure {
                func_id,
                captures,
                body,
                ..
            } => {
                for &cap in captures {
                    if let Some(lidx) = self.index.local_idx(cap)
                        && let Some(ty) = self.read_local_current(inst_id, lidx)
                    {
                        self.invalidate_conservative_cell(ty);
                    }
                }
                let mut refs = Vec::new();
                let mut defined = std::collections::HashSet::new();
                let stmts = if !body.is_empty() {
                    body.as_slice()
                } else if let Some(tid) = self.index.template_of_func(*func_id)
                    && let Some(view) = self.index.view(tid)
                {
                    view.body
                } else {
                    &[]
                };
                for stmt in stmts {
                    perry_hir::analysis::collect_local_refs_stmt(stmt, &mut refs, &mut defined);
                }
                for local_id in refs {
                    if let Some(lidx) = self.index.local_idx(local_id)
                        && let Some(ty) = self.read_local_current(inst_id, lidx)
                    {
                        self.invalidate_conservative_cell(ty);
                    }
                }
                let Some(tid) = self.index.template_of_func(*func_id) else {
                    return Ok(Ty::OPEN);
                };
                let captures_local = captures.iter().any(|cap| {
                    self.index
                        .local_idx(*cap)
                        .is_some_and(|l| !self.index.local_module.contains(l.0 as usize))
                });
                let owner = if captures_local { Some(inst_id) } else { None };
                Ok(self.types.func(FuncIdentity {
                    template: tid,
                    capture_owner: owner,
                }))
            }
            Expr::Binary { op, left, right } => {
                let l = self.eval_expr(inst_id, receiver, left, state)?;
                let r = self.eval_expr(inst_id, receiver, right, state)?;
                Ok(builtins::binary_result(self.ctx, self.types, *op, l, r))
            }
            Expr::Unary { op, operand } => {
                let v = self.eval_expr(inst_id, receiver, operand, state)?;
                Ok(builtins::unary_result(*op, v))
            }
            Expr::Logical { op, left, right } => {
                let l = self.eval_expr(inst_id, receiver, left, state)?;
                let depth = state.guards.depth();
                if matches!(op, LogicalOp::And | LogicalOp::Or) {
                    state.guards.exclude_absences(left, *op == LogicalOp::And);
                }
                let r = self.eval_expr(inst_id, receiver, right, state)?;
                state.guards.truncate(depth);
                Ok(builtins::logical_result(self.types, *op, l, r))
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => {
                self.eval_expr(inst_id, receiver, condition, state)?;
                let guard = DefinedGuard::of(condition);
                let branch = state.guards.branch();
                state.guards.exclude_absences(condition, true);
                let mut then_ty = self.eval_expr(inst_id, receiver, then_expr, state)?;
                if guard.as_ref().is_some_and(|g| g.proves(true, then_expr)) {
                    then_ty = self.types.exclude_absences(then_ty, Absence::Undefined);
                }
                let then_writes = state.guards.other_arm(branch);
                state.guards.exclude_absences(condition, false);
                let mut else_ty = self.eval_expr(inst_id, receiver, else_expr, state)?;
                if guard.as_ref().is_some_and(|g| g.proves(false, else_expr)) {
                    else_ty = self.types.exclude_absences(else_ty, Absence::Undefined);
                }
                state.guards.join(branch, then_writes, true, true);
                Ok(self.types.join(then_ty, else_ty))
            }
            Expr::Sequence(parts) => {
                let mut last = Ty::VOID;
                for part in parts {
                    last = self.eval_expr(inst_id, receiver, part, state)?;
                }
                Ok(last)
            }
            Expr::ScopedTemp { id, value, body } => {
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Some(lidx) = self.index.local_idx(*id) {
                    self.write_local_value(inst_id, lidx, val_ty, state);
                    self.mark_written(inst_id, lidx);
                }
                self.eval_expr(inst_id, receiver, body, state)
            }
            Expr::Await(promise) => {
                let ty = self.eval_expr(inst_id, receiver, promise, state)?;
                Ok(match self.types.kind(ty) {
                    TyKind::Promise(inner) => inner,
                    _ => ty,
                })
            }
            Expr::StructuredClone { value, .. } => self.eval_expr(inst_id, receiver, value, state),
            Expr::Array(elements, _) => {
                if elements.is_empty() {
                    return Ok(Ty::OPEN);
                }
                let mut elem_tys = Vec::with_capacity(elements.len());
                for elem in elements {
                    elem_tys.push(self.eval_expr(inst_id, receiver, elem, state)?);
                }
                if elem_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                if let [first, second] = elem_tys.as_slice() {
                    let (f_def, _) = self.types.strip_nullish(*first);
                    let (s_def, _) = self.types.strip_nullish(*second);
                    if f_def != s_def && !self.types.is_open(*first) && !self.types.is_open(*second)
                    {
                        return Ok(self.types.tuple(&[*first, *second]));
                    }
                }
                let mut joined = Ty::BOTTOM;
                for t in elem_tys {
                    joined = self.types.join(joined, t);
                }
                Ok(self.types.array(joined))
            }
            Expr::ArraySpread(items) => {
                let mut joined = Ty::BOTTOM;
                let mut has_bottom = false;
                for item in items {
                    let t = match item {
                        perry_hir::ir::ArrayElement::Expr(e) => {
                            let v = self.eval_expr(inst_id, receiver, e, state)?;
                            if v == Ty::BOTTOM {
                                has_bottom = true;
                            }
                            v
                        }
                        perry_hir::ir::ArrayElement::Hole => Ty::VOID,
                        perry_hir::ir::ArrayElement::Spread(Expr::Array(elems, _))
                            if elems.is_empty() =>
                        {
                            Ty::NEVER
                        }
                        perry_hir::ir::ArrayElement::Spread(e) => {
                            let s = self.eval_expr(inst_id, receiver, e, state)?;
                            let s = self.resolve_cell_read(inst_id, s);
                            if s == Ty::BOTTOM {
                                has_bottom = true;
                                Ty::BOTTOM
                            } else {
                                builtins::iterated_element(self.ctx, self.types, s)
                                    .unwrap_or(Ty::OPEN)
                            }
                        }
                    };
                    joined = self.types.join(joined, t);
                }
                if has_bottom {
                    return Ok(Ty::BOTTOM);
                }
                // An absence-only array literal like `[, null, undefined]` lowers with a
                // dummy FP64 payload + absence states (matching `perry-lower/src/spread.rs`).
                let (inner, abs) = self.types.strip_nullish(joined);
                if inner == Ty::VOID && abs.is_some_and(|a| a.admits_null()) {
                    joined = self.types.with_absences(Ty::NUMBER, abs);
                }
                Ok(self.types.array(joined))
            }
            Expr::ArrayFrom(source, _) => {
                let s = self.eval_expr(inst_id, receiver, source, state)?;
                let s = self.resolve_cell_read(inst_id, s);
                if s == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                if let Expr::Object(props) = source.as_ref()
                    && props.iter().any(|(k, _)| k == "length")
                {
                    return Ok(self.types.array(Ty::OPEN));
                }
                Ok(builtins::iterated_element(self.ctx, self.types, s)
                    .map_or(Ty::OPEN, |e| self.types.array(e)))
            }
            Expr::Object(fields) => {
                let mut props = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    let vty = self.eval_expr(inst_id, receiver, value, state)?;
                    self.invalidate_conservative_cell(vty);
                    if let TyKind::Func(ident) = self.types.kind(vty) {
                        self.check_escaping_unannotated_func(ident.template)?;
                    }
                    let vty = self.types.normalize_for_storage(self.ctx, self.decls, vty);
                    props.push(ObjectProp {
                        name: self.types.intern_name(name),
                        ty: vty,
                        optional: false,
                        readonly: false,
                    });
                }
                Ok(self.types.object(&props, None))
            }
            Expr::ObjectSpread { parts } => {
                self.eval_object_spread(inst_id, receiver, parts, state)
            }
            Expr::ObjectRest {
                object,
                exclude_keys,
            } => {
                let src = self.eval_expr(inst_id, receiver, object, state)?;
                if src == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(src);
                if stripped == Ty::DYNAMIC {
                    return Ok(Ty::DYNAMIC);
                }
                if let Some(src_props) = self.object_props_of(stripped) {
                    let kept: Vec<ObjectProp> = src_props
                        .into_iter()
                        .filter(|p| {
                            let name = self.types.name_str(p.name);
                            !exclude_keys.iter().any(|k| k == name)
                        })
                        .collect();
                    return Ok(self.types.object(&kept, None));
                }
                Ok(Ty::OPEN)
            }
            Expr::PropertyGet {
                object, property, ..
            } => self.eval_property_get(inst_id, receiver, object, property, state),
            Expr::PropertySet {
                object,
                property,
                value,
                ..
            } => {
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                self.write_class_or_setter(inst_id, obj_def, property, value, val_ty)?;
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
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                let idx_ty = self.eval_expr(inst_id, receiver, index, state)?;
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                if let TyKind::Cell { loc, kind } = self.types.kind(obj_def) {
                    if kind == CellKind::Dict && self.dict_reads.insert(loc.0 as usize) {
                        self.dirty_locs.insert(loc.0 as usize);
                    }
                    if idx_ty != Ty::BOTTOM {
                        let matches_kind = if kind.is_array() {
                            matches!(idx_ty, Ty::NUMBER | Ty::INT32)
                        } else {
                            idx_ty == Ty::STRING
                        };
                        if !matches_kind {
                            self.write_loc(loc, Ty::OPEN);
                        }
                    }
                }
                let obj_ty = self.resolve_cell_read(inst_id, obj_ty);
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
            } => {
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                let idx_ty = self.eval_expr(inst_id, receiver, index, state)?;
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                self.write_cell_indexed(obj_def, idx_ty, val_ty);
                Ok(val_ty)
            }
            Expr::PutValueSet {
                target, key, value, ..
            } => {
                let obj_ty = self.eval_expr(inst_id, receiver, target, state)?;
                let idx_ty = self.eval_expr(inst_id, receiver, key, state)?;
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                if let Expr::String(field_name) = key.as_ref() {
                    self.write_class_or_setter(inst_id, obj_def, field_name, value, val_ty)?;
                }
                self.write_cell_indexed(obj_def, idx_ty, val_ty);
                Ok(val_ty)
            }
            Expr::StaticFieldGet {
                class_name,
                field_name,
            } => {
                let cname_id = self.types.intern_name(class_name);
                let fname_id = self.types.intern_name(field_name);
                if let Some(cid) = self.index.class_by_name_id(cname_id)
                    && let Some(loc) = self.index.class_static_field_loc_by_id(cid, fname_id)
                {
                    return Ok(self.read_loc(loc, inst_id));
                }
                Ok(Ty::OPEN)
            }
            Expr::StaticFieldSet {
                class_name,
                field_name,
                value,
            } => {
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                let cname_id = self.types.intern_name(class_name);
                let fname_id = self.types.intern_name(field_name);
                if let Some(cid) = self.index.class_by_name_id(cname_id)
                    && let Some(loc) = self.index.class_static_field_loc_by_id(cid, fname_id)
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
            } => {
                let arg_tys = self.eval_args(inst_id, receiver, args, state)?;
                if arg_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                let cname_id = self.types.intern_name(class_name);
                let mname_id = self
                    .types
                    .intern_name(hir_compat::method(method_name).name());
                if let Some(cid) = self.index.class_by_name_id(cname_id)
                    && let Some(tid) = self.index.class_static_method_by_id(cid, mname_id)
                {
                    let strip_mask = callable_arg_mask(self.types, &arg_tys);
                    let (_, ret) = self.dispatch_call_site(
                        inst_id,
                        expr,
                        InstanceReq {
                            template: tid,
                            receiver: None,
                            capture_owner: None,
                            args: &arg_tys,
                            explicit_type_args: &[],
                            call_byte_offset: 0,
                            caller: Some(inst_id),
                            is_inline_callback: false,
                        },
                        true,
                        strip_mask,
                    )?;
                    return Ok(ret);
                }
                Ok(Ty::OPEN)
            }
            Expr::New {
                class_name,
                args,
                type_args,
                byte_offset,
                ..
            } => self.eval_new(
                inst_id,
                receiver,
                expr,
                class_name,
                args,
                type_args,
                *byte_offset,
                state,
            ),
            Expr::Call {
                callee,
                args,
                type_args,
                byte_offset,
                ..
            } => self.eval_call(
                inst_id,
                receiver,
                expr,
                callee,
                args,
                type_args,
                *byte_offset,
                state,
            ),
            Expr::CallSpread { callee, args, .. } => {
                self.eval_call_spread(inst_id, receiver, expr, callee, args, state)
            }
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
                    for arg in args {
                        self.eval_expr(inst_id, receiver, arg, state)?;
                    }
                    return Ok(self.types.named(self.ctx, "WritableStream"));
                }
                if module == "__perry_runtime" && method == "requireObjectCoercible" {
                    if let Some(first) = args.first() {
                        let ty = self.eval_expr(inst_id, receiver, first, state)?;
                        return Ok(self.types.strip_nullish(ty).0);
                    }
                    return Ok(Ty::OPEN);
                }
                let arg_tys = self.eval_args(inst_id, receiver, args, state)?;
                if let Some(obj) = object {
                    let obj_ty = self.eval_expr(inst_id, receiver, obj, state)?;
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
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                if obj_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let obj_ty = self.resolve_cell_read(inst_id, obj_ty);
                let (stripped, _) = self.types.strip_nullish(obj_ty);
                let hir = self.types.materialize_hir(self.ctx, stripped, &|_| None);
                let entries = matches!(expr, Expr::ObjectEntries(_));
                Ok(self
                    .decls
                    .dict_entry_types(self.ctx, &hir)
                    .map_or(Ty::OPEN, |(k, v)| {
                        let v = self.types.intern_hir(self.ctx, &v, true);
                        let elem = if entries {
                            let k = self.types.intern_hir(self.ctx, &k, true);
                            self.types.tuple(&[k, v])
                        } else {
                            v
                        };
                        self.types.array(elem)
                    }))
            }
            Expr::ObjectFromEntries(entries) => {
                let ent_ty = self.eval_expr(inst_id, receiver, entries, state)?;
                if ent_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(ent_ty);
                if let TyKind::Array(inner) = self.types.kind(stripped)
                    && let TyKind::Tuple(span) = self.types.kind(inner)
                    && let [_, val] = self.types.tuple_slice(span)
                {
                    let val = *val;
                    let base =
                        shapes::TypeId::from_owned(self.ctx, HirType::Named("Record".into()));
                    return Ok(self.types.generic(base, &[Ty::STRING, val]));
                }
                Ok(Ty::OPEN)
            }
            Expr::ArraySlice {
                array, start, end, ..
            } => {
                let arr_ty = self.eval_expr(inst_id, receiver, array, state)?;
                self.eval_expr(inst_id, receiver, start, state)?;
                if let Some(e) = end {
                    self.eval_expr(inst_id, receiver, e, state)?;
                }
                Ok(arr_ty)
            }
            Expr::ArrayToReversed { array }
            | Expr::ArraySort { array, .. }
            | Expr::ArrayToSorted { array, .. } => self.eval_expr(inst_id, receiver, array, state),
            Expr::ArrayPop(id) | Expr::ArrayShift(id) => {
                let arr_ty = self.eval_local_get(inst_id, *id, &state.guards)?;
                let arr_ty = self.resolve_cell_read(inst_id, arr_ty);
                if arr_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    crate::methods::MethodResult::Element,
                    MethodResultArgs::receiver_only(arr_ty),
                ))
            }
            Expr::ArrayAt { array, index } => {
                let arr_ty = self.eval_expr(inst_id, receiver, array, state)?;
                self.eval_expr(inst_id, receiver, index, state)?;
                let arr_ty = self.resolve_cell_read(inst_id, arr_ty);
                if arr_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    crate::methods::MethodResult::At,
                    MethodResultArgs::receiver_only(arr_ty),
                ))
            }
            Expr::ArrayWith {
                array,
                index,
                value,
            } => {
                let arr_ty = self.eval_expr(inst_id, receiver, array, state)?;
                self.eval_expr(inst_id, receiver, index, state)?;
                self.eval_expr(inst_id, receiver, value, state)?;
                Ok(arr_ty)
            }
            Expr::ArrayKeys(src)
            | Expr::MapKeys(src)
            | Expr::ArrayValues(src)
            | Expr::MapValues(src)
            | Expr::SetValues(src)
            | Expr::ArrayEntries(src)
            | Expr::MapEntries(src) => {
                let s = self.eval_expr(inst_id, receiver, src, state)?;
                let s = self.resolve_cell_read(inst_id, s);
                if s == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let iter_method = match expr {
                    Expr::ArrayKeys(_) | Expr::MapKeys(_) => {
                        crate::collections::IteratorMethod::Keys
                    }
                    Expr::ArrayValues(_) | Expr::MapValues(_) | Expr::SetValues(_) => {
                        crate::collections::IteratorMethod::Values
                    }
                    _ => crate::collections::IteratorMethod::Entries,
                };
                Ok(
                    builtins::iterator_view(self.ctx, self.types, s, iter_method)
                        .unwrap_or(Ty::OPEN),
                )
            }
            Expr::MapGet { map, key } => {
                let m = self.eval_expr(inst_id, receiver, map, state)?;
                self.eval_expr(inst_id, receiver, key, state)?;
                if m == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    crate::methods::MethodResult::Lookup,
                    MethodResultArgs::receiver_only(m),
                ))
            }
            Expr::MapSet { map, key, value } => {
                let m = self.eval_expr(inst_id, receiver, map, state)?;
                self.eval_expr(inst_id, receiver, key, state)?;
                self.eval_expr(inst_id, receiver, value, state)?;
                Ok(m)
            }
            Expr::SetAdd { set_id, value } => {
                self.eval_expr(inst_id, receiver, value, state)?;
                self.eval_local_get(inst_id, *set_id, &state.guards)
            }
            Expr::CryptoRandomFillSync {
                buffer,
                offset,
                size,
            } => {
                let b = self.eval_expr(inst_id, receiver, buffer, state)?;
                self.eval_expr(inst_id, receiver, offset, state)?;
                self.eval_expr(inst_id, receiver, size, state)?;
                Ok(b)
            }
            Expr::Compare { op, left, right }
                if matches!(
                    op,
                    CompareOp::Eq | CompareOp::Ne | CompareOp::LooseEq | CompareOp::LooseNe
                ) =>
            {
                let l = self.eval_expr(inst_id, receiver, left, state)?;
                let r = self.eval_expr(inst_id, receiver, right, state)?;
                if let (TyKind::Func(a), TyKind::Func(b)) = (self.types.kind(l), self.types.kind(r))
                {
                    self.fold_func_compare(inst_id, expr, *op, a, b, left, right)?;
                }
                Ok(Ty::BOOLEAN)
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => {
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Some(lidx) = self.index.local_idx(*array_id)
                    && let Some(arr_ty) = self.read_local_value(inst_id, lidx)
                {
                    let (def, _) = self.types.strip_nullish(arr_ty);
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
                    let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                    if self.type_may_be_array(obj_ty) {
                        self.deletes_array_elements = true;
                    }
                } else {
                    self.eval_children(inst_id, receiver, expr, state)?;
                }
                Ok(Ty::BOOLEAN)
            }
            Expr::Update { id, .. } => {
                state.guards.assign(*id);
                if let Some(lidx) = self.index.local_idx(*id) {
                    self.mark_written(inst_id, lidx);
                }
                Ok(Ty::NUMBER)
            }
            _ => {
                // Leaf/intrinsic expression: evaluate child expressions for side effects and
                // delegate typing to `builtins::leaf_expr_type` (reusing `perry-hir`).
                self.eval_children(inst_id, receiver, expr, state)?;
                Ok(builtins::leaf_expr_type(self.ctx, self.types, expr))
            }
        }
    }

    fn eval_children(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<()> {
        let mut err = None;
        perry_hir::walker::walk_expr_children(expr, &mut |child| {
            if err.is_none()
                && let Err(e) = self.eval_expr(inst_id, receiver, child, state)
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
                || self.index.view(ident.template).is_some_and(|v| {
                    v.captures.iter().any(|c| {
                        self.index
                            .local_idx(*c)
                            .is_some_and(|li| !self.index.local_module.contains(li.0 as usize))
                    })
                })
            {
                let raw_name = self.index.view(ident.template).map_or("", |v| v.name);
                let binding_name = if raw_name.is_empty()
                    && let Expr::LocalGet(lid) = side
                    && let Some(span) = self.hir.local_source_spans.get(lid)
                    && let Some(src) = self.metadata.locations.source_at(span.start)
                {
                    src.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
                        .next()
                        .unwrap_or("closure")
                } else if raw_name.is_empty() {
                    "closure"
                } else {
                    raw_name
                };
                let err = Diagnostic::unsupported(format!(
                    "cannot compare capturing function '{binding_name}'"
                ));
                return Err(match self.facts.callable_comparisons.first() {
                    Some(&span) => self.metadata.locations.annotate(err, span),
                    None => err,
                });
            }
        }
        let eq = (a.template == b.template) == matches!(op, CompareOp::Eq | CompareOp::LooseEq);
        self.record_site(inst_id, expr, SiteResolution::FoldBool(eq));
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
        if matches!(value, Expr::Array(items, _) if items.is_empty()) {
            self.write_class_field_empty_array(cinst_id, property);
        } else {
            self.write_class_field(cinst_id, property, norm);
        }
        let prop_id = self.types.intern_name(property);
        if let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
            && let Some(setter_tid) = self.index.class_setter_by_id(cinst.class, prop_id)
            && norm != Ty::BOTTOM
        {
            self.request_instance(InstanceReq {
                template: setter_tid,
                receiver: Some(cinst_id),
                capture_owner: None,
                args: &[norm],
                explicit_type_args: &[],
                call_byte_offset: 0,
                caller: Some(inst_id),
                is_inline_callback: false,
            })?;
        }
        Ok(())
    }

    pub(super) fn eval_local_get(
        &mut self,
        inst_id: InstanceId,
        id: LocalId,
        guards: &Guards<'h>,
    ) -> Result<Ty> {
        let Some(lidx) = self.index.local_idx(id) else {
            return Ok(Ty::OPEN);
        };
        let Some(mut ty) = self.read_local_value(inst_id, lidx) else {
            return Ok(self
                .index
                .local_declared
                .get(lidx.0 as usize)
                .copied()
                .unwrap_or(Ty::OPEN));
        };
        // Deferred local read before any write sees `undefined`.
        if self.index.local_deferred.contains(lidx.0 as usize)
            && self
                .inst_written
                .get(inst_id.0 as usize)
                .is_none_or(|w| !w.contains(lidx.0 as usize))
            && ty == Ty::BOTTOM
        {
            return Ok(Ty::VOID);
        }
        // Apply flow-narrowing guards when the local is not written from a closure.
        if !self
            .index
            .local_written_from_closure
            .contains(lidx.0 as usize)
            && let Some(excluded) = guards.excluded(id)
        {
            ty = self.types.exclude_absences(ty, excluded);
        }
        Ok(ty)
    }

    pub(super) fn eval_collection_constructor(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // Extract explicit types from metadata first so `init` keeps its `'h` borrow of `expr`.
        let (is_map, explicit_tys) = match self.metadata.constructors.get(expr) {
            Some(hir_compat::Constructor::Map { types, .. }) => (
                true,
                types.map(|(k, v)| {
                    vec![
                        self.types.intern_hir(self.ctx, k, true),
                        self.types.intern_hir(self.ctx, v, true),
                    ]
                }),
            ),
            Some(hir_compat::Constructor::Set { element, .. }) => (
                false,
                element.map(|e| vec![self.types.intern_hir(self.ctx, e, true)]),
            ),
            None => return Ok(Ty::OPEN),
        };
        let init = constructor_init_expr(expr);
        if let Some(init) = init {
            self.eval_expr(inst_id, receiver, init, state)?;
        }
        let base_name = if is_map { "Map" } else { "Set" };
        let base = shapes::TypeId::from_owned(self.ctx, HirType::Named(base_name.into()));
        if let Some(mut arg_tys) = explicit_tys {
            let type_bindings = self.active_instance_type_bindings(inst_id);
            for ty in &mut arg_tys {
                *ty = self.types.substitute(self.ctx, *ty, &type_bindings);
            }
            if !type_bindings.is_empty() {
                self.record_site(
                    inst_id,
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
                    let k = self.eval_expr(inst_id, receiver, key, state)?;
                    let v = self.eval_expr(inst_id, receiver, value, state)?;
                    if k == Ty::BOTTOM || v == Ty::BOTTOM {
                        return Ok(Ty::BOTTOM);
                    }
                    return Ok(self.types.generic(base, &[k, v]));
                }
                let init_ty = self.eval_expr(inst_id, receiver, init, state)?;
                if init_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(init_ty);
                if let TyKind::Array(elem) = self.types.kind(stripped)
                    && let TyKind::Tuple(span) = self.types.kind(elem)
                    && let [k, v] = self.types.tuple_slice(span)
                {
                    let (k, v) = (*k, *v);
                    return Ok(self.types.generic(base, &[k, v]));
                }
            } else {
                let init_ty = self.eval_expr(inst_id, receiver, init, state)?;
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
        let template = inst.template;
        let receiver = inst.receiver;
        let capture_owner = inst.capture_owner;
        let args: Vec<Ty> = self
            .instance_args
            .get(inst.args.range())
            .unwrap_or(&[])
            .to_vec();
        let explicit_targs: Vec<Ty> = self
            .instance_type_args
            .get(inst.explicit_type_args.range())
            .unwrap_or(&[])
            .to_vec();
        self.compute_type_param_bindings(template, receiver, capture_owner, &args, &explicit_targs)
    }

    pub(super) fn eval_property_get(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        object: &'h Expr,
        property: &str,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        if let Expr::ClassRef(class_name) = object {
            let cname_id = self.types.intern_name(class_name);
            let pname_id = self.types.intern_name(property);
            if let Some(cid) = self.index.class_by_name_id(cname_id)
                && let Some(loc) = self.index.class_static_field_loc_by_id(cid, pname_id)
            {
                return Ok(self.read_loc(loc, inst_id));
            }
            return Ok(Ty::OPEN);
        }
        let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
        if obj_ty == Ty::BOTTOM {
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        if let TyKind::Cell {
            loc,
            kind: CellKind::Dict,
        } = self.types.kind(stripped)
            && self.dict_reads.insert(loc.0 as usize)
        {
            self.dirty_locs.insert(loc.0 as usize);
        }
        let obj_ty = if matches!(
            self.types.kind(stripped),
            TyKind::Cell {
                kind: CellKind::Dict,
                ..
            }
        ) {
            self.resolve_cell_read(inst_id, obj_ty)
        } else {
            obj_ty
        };
        let (stripped, _) = self.types.strip_nullish(obj_ty);

        // Class instance field or getter!
        if let Some(cinst_id) = self.resolve_class_inst(inst_id, stripped, 0)? {
            if let Some(ty) = self.read_class_field(cinst_id, property, inst_id) {
                return Ok(ty);
            }
            let pname_id = self.types.intern_name(property);
            if let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
                && let Some(getter_tid) = self.index.class_getter_by_id(cinst.class, pname_id)
            {
                let target = self.request_instance(InstanceReq {
                    template: getter_tid,
                    receiver: Some(cinst_id),
                    capture_owner: None,
                    args: &[],
                    explicit_type_args: &[],
                    call_byte_offset: 0,
                    caller: Some(inst_id),
                    is_inline_callback: false,
                })?;
                let ret_loc = self.instances[target.0 as usize].return_loc;
                return Ok(self.read_loc(ret_loc, inst_id));
            }
        }

        Ok(
            builtins::property_type(self.ctx, self.decls, self.types, obj_ty, property)
                .unwrap_or(Ty::OPEN),
        )
    }

    fn class_field_loc(&self, cinst_id: ClassInstId, property: &str) -> Option<LocId> {
        let cinst = self.class_insts.get(cinst_id.0 as usize)?;
        let hir_idx = *self.index.class_hir_idx.get(cinst.class.0 as usize)?;
        let f_idx = self
            .hir
            .classes
            .get(hir_idx)?
            .fields
            .iter()
            .position(|f| f.name == property)?;
        Some(LocId(cinst.field_loc_base + f_idx as u32))
    }

    pub(super) fn read_class_field(
        &mut self,
        cinst_id: ClassInstId,
        property: &str,
        reader: InstanceId,
    ) -> Option<Ty> {
        Some(self.read_loc(self.class_field_loc(cinst_id, property)?, reader))
    }

    pub(super) fn write_class_field(&mut self, cinst_id: ClassInstId, property: &str, value: Ty) {
        if let Some(loc) = self.class_field_loc(cinst_id, property) {
            let existing = self.locations[loc.0 as usize];
            if !self.types.is_open(value) || self.types.is_open(existing) {
                self.write_loc(loc, value);
            }
        }
    }

    pub(super) fn write_class_field_empty_array(&mut self, cinst_id: ClassInstId, property: &str) {
        if let Some(loc) = self.class_field_loc(cinst_id, property) {
            let existing = self.locations[loc.0 as usize];
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
        }
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
            let resolved_props = self.decls.object_properties(self.ctx, &hir)?;
            let mut out = Vec::with_capacity(resolved_props.len());
            for (field_name, field_hir, optional) in resolved_props {
                let name = self.types.intern_name(&field_name);
                let mut field_ty = self.types.intern_hir(self.ctx, &field_hir, true);
                if optional {
                    field_ty = self.types.with_absences(field_ty, Absence::Undefined);
                }
                out.push(ObjectProp {
                    name,
                    ty: field_ty,
                    optional,
                    readonly: false,
                });
            }
            return Some(out);
        }
        None
    }

    pub(super) fn eval_object_spread(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        parts: &'h [perry_hir::ir::ObjectPart],
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        let mut props: Vec<ObjectProp> = Vec::new();
        let mut is_dynamic = false;
        let mut has_bottom = false;
        for part in parts {
            let val_expr = part.value();
            let val_ty = self.eval_expr(inst_id, receiver, val_expr, state)?;
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
                if let Some(existing) = props.iter_mut().find(|p| p.name == name) {
                    existing.ty = match val_expr {
                        Expr::Null => self.types.with_absences(existing.ty, Absence::Null),
                        Expr::Undefined => {
                            self.types.with_absences(existing.ty, Absence::Undefined)
                        }
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
                if let Some(src_props) = self.object_props_of(stripped) {
                    for sp in src_props {
                        if let Some(existing) = props.iter_mut().find(|p| p.name == sp.name) {
                            *existing = sp;
                        } else {
                            props.push(sp);
                        }
                    }
                }
            }
        }
        if has_bottom {
            return Ok(Ty::BOTTOM);
        }
        if is_dynamic {
            return Ok(Ty::DYNAMIC);
        }
        Ok(self.types.object(&props, None))
    }
}
