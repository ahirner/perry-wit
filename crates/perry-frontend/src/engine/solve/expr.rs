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
                    if let Some(existing) = self.read_local_value(s.inst, lidx) {
                        let is_inferred = self.index.is_inferred_local(self.hir, *id, lidx);
                        let local_span = self.hir.local_source_spans.get(id).map(|sp| {
                            swc_common::Span::new(
                                swc_common::BytePos(sp.start),
                                swc_common::BytePos(sp.end),
                            )
                        });
                        if matches!(self.types.kind(existing), TyKind::Func(_))
                            || self.is_tagged_func_union_in(existing, s.inst)
                        {
                            self.check_escaping_func(
                                s.inst,
                                val_ty,
                                0,
                                s.current_span.or(local_span),
                            )?;
                            let joined = self.types.join(existing, val_ty);
                            self.check_escaping_func(
                                s.inst,
                                joined,
                                0,
                                s.current_span.or(local_span),
                            )?;
                            self.write_local_value(s, lidx, val_ty);
                        } else if let TyKind::Cell { loc, kind } = self.types.kind(existing) {
                            if kind == CellKind::Array {
                                let (def, _) = self.types.strip_nullish(val_ty);
                                match self.types.kind(def) {
                                    TyKind::Array(elem) => self.write_cell(loc, elem),
                                    TyKind::Cell { loc: src, kind: sk } if sk.is_array() => {
                                        let elem = self.read_loc(src, s.inst);
                                        self.write_cell(loc, elem);
                                    }
                                    _ if val_ty != Ty::BOTTOM => {
                                        self.write_loc(loc, Ty::OPEN);
                                    }
                                    _ => {}
                                }
                            } else {
                                self.write_loc(loc, Ty::OPEN);
                            }
                        } else if (is_inferred
                            && (self.index.local_deferred.contains(lidx.index())
                                || self.index.local_nullish_init.contains(lidx.index())))
                            || self.types.is_open(existing)
                        {
                            self.check_escaping_func(
                                s.inst,
                                val_ty,
                                0,
                                s.current_span.or(local_span),
                            )?;
                            let norm = self
                                .types
                                .normalize_for_storage(self.ctx, self.decls, val_ty);
                            self.write_local_value(s, lidx, norm);
                        }
                    }
                    if let Some(cur) = self.read_local_value(s.inst, lidx) {
                        self.record_coercion(s.inst, value, val_ty, cur);
                    }
                    self.mark_written(s.inst, lidx);
                }
                Ok(val_ty)
            }
            Expr::PrivateGuard {
                class_id,
                object,
                field_name,
                kind: 0,
                ..
            } => {
                let ty = self.eval_expr(s, object)?;
                if ty == Ty::BOTTOM
                    || matches!(self.types.kind(ty), TyKind::ClassInst(id)
                    if self.hir.classes.get(self.index.classes[self.class_insts[id].class].hir_idx)
                        .is_some_and(|class| class.id == *class_id))
                {
                    return Ok(ty);
                }
                Err(Diagnostic::unsupported(format!(
                    "receiver of private field '{field_name}' must have its declaring class"
                )))
            }
            Expr::This => Ok(match s.receiver {
                Some(Receiver::Class(c)) => self.types.class_inst(c),
                Some(Receiver::Object(ty)) => ty,
                None => Ty::OPEN,
            }),
            Expr::FuncRef(fid) => {
                Ok(self
                    .index
                    .template_of_func(*fid)
                    .map_or(Ty::OPEN, |template| {
                        self.types.func(FuncIdentity {
                            template,
                            capture_owner: CaptureOwner::None,
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
                        .and_then(|l| self.read_local_value(s.inst, l))
                    {
                        self.invalidate_conservative_cell(ty);
                    }
                }
                let Some(template) = self.index.template_of_func(*func_id) else {
                    return Ok(Ty::OPEN);
                };
                let capture_owner = if self.index.template_captures(template).is_empty() {
                    CaptureOwner::None
                } else if let Some(owner_inst) = self.instances.get(s.inst) {
                    let owner_tid = owner_inst.template;
                    let owner_args = owner_inst.args;
                    let self_nested = self.collapsed_closures.contains(template.index())
                        || self
                            .types
                            .contains_func_template(owner_args, template, None)
                        || self
                            .index
                            .template_captures(template)
                            .iter()
                            .any(|&cap_id| {
                                self.index
                                    .local_idx(cap_id)
                                    .and_then(|lidx| self.read_local_value(s.inst, lidx))
                                    .is_some_and(|ty| {
                                        self.types.contains_func_template(
                                            ty,
                                            template,
                                            Some(CaptureOwner::Instance(s.inst)),
                                        )
                                    })
                            });
                    if self_nested {
                        self.collapse_closure(template, owner_tid);
                        CaptureOwner::Template(owner_tid)
                    } else {
                        CaptureOwner::Instance(s.inst)
                    }
                } else {
                    CaptureOwner::Instance(s.inst)
                };
                if self.is_escaping_env(template, s.inst) {
                    let is_collapsed = matches!(capture_owner, CaptureOwner::Template(_));
                    for &cap_id in self.index.template_captures(template) {
                        if let Some(lidx) = self.index.local_idx(cap_id)
                            && let Some(cap_ty) = self.read_local_value(s.inst, lidx)
                        {
                            self.mark_escaping_env(s.inst, cap_ty);
                            if is_collapsed
                                && let Some(cloc) = self
                                    .collapsed_local_locs
                                    .get(lidx.index())
                                    .copied()
                                    .flatten()
                            {
                                let dst_ty = self.read_loc(cloc, s.inst);
                                if cap_ty != dst_ty
                                    && self.is_tagged_func_union_in(cap_ty, s.inst)
                                    && self.is_tagged_func_union_in(dst_ty, s.inst)
                                {
                                    self.intern_helper(
                                        HelperSpec::Widener {
                                            from: cap_ty,
                                            to: dst_ty,
                                        },
                                        0,
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(self.types.func(FuncIdentity {
                    template,
                    capture_owner,
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
                let res = builtins::logical_result(self.types, *op, l, r);
                self.check_escaping_func(s.inst, res, 0, s.current_span)?;
                self.record_site(s.inst, expr, SiteResolution::BranchResult(res));
                self.record_coercion(s.inst, left, l, res);
                self.record_coercion(s.inst, right, r, res);
                Ok(res)
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
                let joined = self.types.join(then_ty, else_ty);
                self.check_escaping_func(s.inst, joined, 0, s.current_span)?;
                self.record_site(s.inst, expr, SiteResolution::BranchResult(joined));
                self.record_coercion(s.inst, then_expr, then_ty, joined);
                self.record_coercion(s.inst, else_expr, else_ty, joined);
                Ok(joined)
            }
            Expr::Sequence(parts) => {
                if let Some(span) = self.metadata.locations.operator_raw_span(parts)
                    && let Some(last) = parts.last()
                {
                    let prev = s.current_span;
                    s.current_span = Some(span);
                    let res = self.eval_expr(s, last);
                    s.current_span = prev;
                    return res;
                }
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
                let ty = self.resolve_cell_read(s.inst, ty);
                Ok(match self.types.kind(ty) {
                    TyKind::Promise(i) => i,
                    _ => ty,
                })
            }
            Expr::QueueMicrotask(_) => Err(self.annotate_expr_error(
                s,
                Diagnostic::unsupported(
                    "queueMicrotask is not supported: this runtime has no microtask queue",
                ),
                0,
            )),
            Expr::ProcessNextTick { .. } => Err(self.annotate_expr_error(
                s,
                Diagnostic::unsupported(
                    "process.nextTick is not supported: this runtime has no microtask queue",
                ),
                0,
            )),
            Expr::StructuredClone { value, .. } => self.eval_expr(s, value),
            Expr::Array(elements, _) => {
                if elements.is_empty() {
                    return self.alloc_expr_cell(s.inst, expr, CellKind::Array);
                }
                let elem_tys = self.eval_args(s, elements)?;
                if elem_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                for &et in &elem_tys {
                    self.check_escaping_func(s.inst, et, 0, s.current_span)?;
                }
                if let [a, b] = elem_tys.as_slice()
                    && self.types.strip_nullish(*a).0 != self.types.strip_nullish(*b).0
                    && !self.types.is_open(*a)
                    && !self.types.is_open(*b)
                    && !(self.types.has_func(*a) && self.types.has_func(*b))
                {
                    return Ok(self.types.tuple(&[*a, *b]));
                }
                let joined = elem_tys
                    .iter()
                    .copied()
                    .fold(Ty::BOTTOM, |acc, t| self.types.join(acc, t));
                self.check_escaping_func(s.inst, joined, 0, s.current_span)?;
                let arr_ty = self.types.array(joined);
                if self
                    .types
                    .is_zero_sized_func(joined, &self.index.capture_free_templates)
                {
                    self.record_site(s.inst, expr, SiteResolution::ZeroSizedArray);
                } else if self.is_tagged_func_union_in(joined, s.inst) {
                    for (elem, &et) in elements.iter().zip(&elem_tys) {
                        self.record_coercion(s.inst, elem, et, joined);
                    }
                    self.record_site(s.inst, expr, SiteResolution::TypedArray(arr_ty));
                }
                Ok(arr_ty)
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
                let array = self.types.array(joined);
                self.record_site(s.inst, expr, SiteResolution::TypedArray(array));
                Ok(array)
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
                self.eval_object_fields(s, expr, fields.iter().map(|(n, v)| (n.as_str(), v)))
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
                object,
                property,
                byte_offset,
                ..
            } => self.eval_property_get(s, object, property, *byte_offset),
            Expr::PropertySet {
                object,
                property,
                value,
                byte_offset,
                ..
            } => {
                let (obj_ty, val_ty) = (self.eval_expr(s, object)?, self.eval_expr(s, value)?);
                self.check_escaping_func(s.inst, val_ty, *byte_offset, s.current_span)?;
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                let objlit_prop = self.check_objlit_func_write(
                    s.inst,
                    obj_def,
                    property,
                    val_ty,
                    *byte_offset,
                    s.current_span,
                )?;
                self.write_class_or_setter(s.inst, obj_def, property, value, val_ty)?;
                let field_ty = if let TyKind::ClassInst(cinst_id) = self.types.kind(obj_def)
                    && let Some(loc) = self.class_field_loc(cinst_id, property)
                {
                    self.read_loc(loc, s.inst)
                } else {
                    objlit_prop.unwrap_or(val_ty)
                };
                if self
                    .types
                    .is_zero_sized_func(field_ty, &self.index.capture_free_templates)
                {
                    self.record_site(s.inst, expr, SiteResolution::ZeroSizedFieldSet);
                } else {
                    self.clear_site(s.inst, expr);
                    self.record_coercion(s.inst, value, val_ty, field_ty);
                }
                if let TyKind::Cell { loc, kind } = self.types.kind(obj_def) {
                    if kind == CellKind::Dict {
                        self.write_cell(loc, val_ty);
                    } else {
                        self.write_loc(loc, Ty::OPEN);
                    }
                }
                Ok(val_ty)
            }
            Expr::IndexGet {
                object,
                index,
                element_offset,
                ..
            } => {
                let obj_ty = self.eval_expr(s, object)?;
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                self.check_timer_member(obj_def, *element_offset)?;
                let idx_ty = self.eval_expr(s, index)?;
                if let TyKind::Cell { loc, kind } = self.types.kind(obj_def) {
                    if kind == CellKind::Dict && self.dict_reads.insert(loc.index()) {
                        self.dirty_locs.insert(loc.index());
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
                if let Expr::PutValueSet { byte_offset, .. } = expr
                    && let Expr::String(field_name) = index.as_ref()
                {
                    self.check_escaping_func(s.inst, val_ty, *byte_offset, s.current_span)?;
                    let objlit_prop = self.check_objlit_func_write(
                        s.inst,
                        obj_def,
                        field_name,
                        val_ty,
                        *byte_offset,
                        s.current_span,
                    )?;
                    self.write_class_or_setter(s.inst, obj_def, field_name, value, val_ty)?;
                    let field_ty = if let TyKind::ClassInst(cinst_id) = self.types.kind(obj_def)
                        && let Some(loc) = self.class_field_loc(cinst_id, field_name)
                    {
                        self.read_loc(loc, s.inst)
                    } else {
                        objlit_prop.unwrap_or(val_ty)
                    };
                    if self
                        .types
                        .is_zero_sized_func(field_ty, &self.index.capture_free_templates)
                    {
                        self.record_site(s.inst, expr, SiteResolution::ZeroSizedFieldSet);
                    } else {
                        self.clear_site(s.inst, expr);
                        self.record_coercion(s.inst, value, val_ty, field_ty);
                    }
                }
                self.write_cell_indexed(obj_def, idx_ty, val_ty);
                self.coerce_array_write(
                    s.inst,
                    obj_def,
                    std::slice::from_ref(value.as_ref()),
                    &[val_ty],
                );
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
            Expr::NewDynamic {
                callee,
                args,
                byte_offset,
                ..
            } => {
                self.eval_expr(s, callee)?;
                let arg_tys = self.eval_args(s, args)?;
                if arg_tys.iter().any(|&a| self.types.has_func(a)) {
                    let name = match callee.as_ref() {
                        Expr::PropertyGet { property, .. } => property.as_str(),
                        Expr::ExternFuncRef { name, .. } => name.as_str(),
                        _ => "constructor",
                    };
                    return Err(self.annotate_expr_error(
                        s,
                        Diagnostic::unsupported(format!(
                            "passing a callback to runtime '{name}' requires the runtime to call a guest function"
                        )),
                        *byte_offset,
                    ));
                }
                Ok(Ty::OPEN)
            }
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
                object,
                method,
                args,
                ..
            } => {
                if hir_compat::is_writable_to_web(expr) {
                    self.eval_args(s, args)?;
                    return Ok(self.types.named(self.ctx, "WritableStream"));
                }
                if hir_compat::is_object_coercibility_check(module, method) {
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
                    if hir_compat::is_array_insertion(method)
                        && self.push_into_cells(stripped, &arg_tys)
                    {
                        return Ok(Ty::NUMBER);
                    }
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
                if arg_tys.iter().any(|&a| self.types.has_func(a)) {
                    return Err(self.annotate_expr_error(
                        s,
                        Diagnostic::unsupported(format!(
                            "passing a callback to runtime '{method}' requires the runtime to call a guest function"
                        )),
                        0,
                    ));
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
            Expr::TypeOf(operand) => {
                let op_ty = self.eval_expr(s, operand)?;
                if op_ty != Ty::BOTTOM && self.types.has_func(op_ty) {
                    let has_nullish = self.types.strip_nullish(op_ty).1.is_some();
                    let helper =
                        has_nullish.then(|| self.intern_helper(HelperSpec::TypeOfFn(op_ty), 0));
                    self.record_site(s.inst, expr, SiteResolution::FuncTypeOf(helper));
                }
                Ok(Ty::STRING)
            }
            Expr::Compare { op, left, right }
                if matches!(
                    op,
                    CompareOp::Eq | CompareOp::Ne | CompareOp::LooseEq | CompareOp::LooseNe
                ) =>
            {
                let (l, r) = (self.eval_expr(s, left)?, self.eval_expr(s, right)?);
                if l != Ty::BOTTOM
                    && r != Ty::BOTTOM
                    && (self.types.has_func(l) || self.types.has_func(r))
                {
                    self.eval_func_compare(s.inst, expr, *op, l, r, left, right, s.current_span)?;
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
                    self.push_into_cells(def, &[val_ty]);
                    self.coerce_array_write(
                        s.inst,
                        def,
                        std::slice::from_ref(value.as_ref()),
                        &[val_ty],
                    );
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
                    self.eval_children(s, expr, None)?;
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
                if let Some((receiver, call)) = crate::methods::Call::expression(expr) {
                    let (ty, evaluated) = match receiver {
                        crate::methods::CallReceiver::Expression(e) => {
                            (self.eval_expr(s, e)?, Some(e))
                        }
                        crate::methods::CallReceiver::Local(id) => {
                            (self.eval_local_get(s, id)?, None)
                        }
                    };
                    self.eval_children(s, expr, evaluated)?;
                    return Ok(self.apply_resolved_method(s.inst, ty, call.result_rule()));
                }
                self.eval_children(s, expr, None)?;
                Ok(builtins::leaf_expr_type(self.ctx, self.types, expr))
            }
        }
    }

    fn eval_children(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        evaluated: Option<&Expr>,
    ) -> Result<()> {
        let mut result = Ok(());
        perry_hir::walker::walk_expr_children(expr, &mut |child| {
            if result.is_ok() && !evaluated.is_some_and(|e| std::ptr::eq(e, child)) {
                result = self.eval_expr(s, child).map(drop);
            }
        });
        result
    }

    fn check_objlit_func_write(
        &mut self,
        inst_id: InstanceId,
        obj_def: Ty,
        property: &str,
        val_ty: Ty,
        byte_offset: u32,
        span: Option<swc_common::Span>,
    ) -> Result<Option<Ty>> {
        if !matches!(self.types.kind(obj_def), TyKind::Object { .. }) {
            return Ok(None);
        }
        let prop_ty = builtins::property_type(self.ctx, self.decls, self.types, obj_def, property);
        let touches_func =
            self.types.has_func(val_ty) || prop_ty.is_some_and(|p| self.types.has_func(p));
        if touches_func
            && val_ty != Ty::BOTTOM
            && !prop_ty.is_some_and(|p| self.types.accepts(self.ctx, p, val_ty))
        {
            let mut err = Diagnostic::unsupported(format!(
                "reassigning function property '{property}' on an object literal is not supported yet"
            ));
            if hir_compat::has_source_offset(byte_offset) {
                if let Some(sp) = self.metadata.locations.member_span(byte_offset) {
                    err = err.with_span(sp);
                }
            } else if let Some(sp) = span {
                err = self.metadata.locations.annotate(err, sp);
            }
            return Err(attach_inst_chain(
                &self.instances,
                self.index,
                &self.metadata.locations,
                err,
                inst_id,
            ));
        }
        Ok(prop_ty)
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_func_compare(
        &mut self,
        inst_id: InstanceId,
        expr: &'h Expr,
        op: CompareOp,
        l: Ty,
        r: Ty,
        left: &Expr,
        right: &Expr,
        span: Option<swc_common::Span>,
    ) -> Result<()> {
        if matches!(l, Ty::NULL | Ty::VOID) || matches!(r, Ty::NULL | Ty::VOID) {
            return Ok(());
        }
        self.mark_escaping_env(inst_id, l);
        self.mark_escaping_env(inst_id, r);
        let same_local = matches!((left, right), (Expr::LocalGet(a), Expr::LocalGet(b)) if a == b);
        if !same_local {
            let shared: Vec<_> = self
                .types
                .func_members(&l)
                .map(|ident| ident.template)
                .filter(|&template| {
                    self.types
                        .func_members(&r)
                        .any(|ident| ident.template == template)
                })
                .collect();
            for tid in shared {
                if !self.is_escaping_env(tid, inst_id)
                    && self
                        .index
                        .view(tid)
                        .is_some_and(|v| v.kind == TemplateKind::Closure)
                    && (self.index.closure_in_loop.contains(tid.index())
                        || self
                            .index
                            .closure_owner_template
                            .get(tid.index())
                            .copied()
                            .flatten()
                            .is_some_and(|owner| owner != TemplateId::INIT))
                {
                    let err = Diagnostic::unsupported(
                        "cannot compare a capture-free closure literal that may be evaluated more than once",
                    );
                    let err = match span {
                        Some(sp) => self.metadata.locations.annotate(err, sp),
                        None => err,
                    };
                    return Err(attach_inst_chain(
                        &self.instances,
                        self.index,
                        &self.metadata.locations,
                        err,
                        inst_id,
                    ));
                }
            }
        }
        let is_eq = matches!(op, CompareOp::Eq | CompareOp::LooseEq);
        if let (TyKind::Func(a), TyKind::Func(b)) = (self.types.kind(l), self.types.kind(r)) {
            let a_zero = self
                .types
                .is_zero_sized_func(l, &self.index.capture_free_templates);
            let b_zero = self
                .types
                .is_zero_sized_func(r, &self.index.capture_free_templates);
            if a_zero && b_zero {
                self.record_site(
                    inst_id,
                    expr,
                    SiteResolution::FoldBool((a.template == b.template) == is_eq),
                );
                return Ok(());
            }
            if a != b {
                self.record_site(inst_id, expr, SiteResolution::FoldBool(!is_eq));
                return Ok(());
            }
        }
        let l_zero = self
            .types
            .is_zero_sized_func(l, &self.index.capture_free_templates);
        let r_zero = self
            .types
            .is_zero_sized_func(r, &self.index.capture_free_templates);
        let l_env = self.types.is_env_func(l, &self.escaping_env_templates);
        let r_env = self.types.is_env_func(r, &self.escaping_env_templates);
        let helper = (!(l_zero || r_zero || (l_env && r_env && l == r))).then(|| {
            self.intern_helper(
                HelperSpec::Comparer {
                    left_ty: l,
                    right_ty: r,
                },
                0,
            )
        });
        self.record_site(
            inst_id,
            expr,
            SiteResolution::FuncCompare {
                left_ty: l,
                right_ty: r,
                op,
                helper,
            },
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
            let existing = self.locations[loc];
            if matches!(value, Expr::Array(items, _) if items.is_empty()) {
                if !matches!(
                    self.types.kind(existing),
                    TyKind::Cell {
                        kind: CellKind::Array,
                        ..
                    }
                ) && self.types.is_open(existing)
                {
                    let cell = self.alloc_expr_cell(inst_id, value, CellKind::Array)?;
                    self.write_loc(loc, cell);
                }
            } else if !self.types.is_open(norm) || self.types.is_open(existing) {
                self.write_loc(loc, norm);
            }
        }
        let prop_id = self.types.intern_name(property);
        if norm != Ty::BOTTOM
            && let Some(cinst) = self.class_insts.get(cinst_id)
            && let Some(setter_tid) = self
                .index
                .class(cinst.class)
                .and_then(|c| ClassInfo::find(&c.setters, prop_id))
        {
            self.request_instance(
                InstanceReq::new(setter_tid, &[norm], Some(inst_id))
                    .receiver(Some(Receiver::Class(cinst_id))),
            )?;
        }
        Ok(())
    }

    pub(super) fn eval_local_get(&mut self, s: &mut EvalState<'h>, id: LocalId) -> Result<Ty> {
        let Some(lidx) = self.index.local_idx(id) else {
            return Ok(Ty::OPEN);
        };
        let Some(mut ty) = self.read_local_value(s.inst, lidx) else {
            return Ok(self.index.local_declared[lidx]);
        };
        if self.index.local_deferred.contains(lidx.index())
            && self
                .inst_written
                .get(s.inst.index())
                .is_none_or(|w| !w.contains(lidx.index()))
            && ty == Ty::BOTTOM
        {
            return Ok(Ty::VOID);
        }
        if !self.index.local_written_from_closure.contains(lidx.index())
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
            let mut specialized = false;
            for ty in &mut arg_tys {
                let bound = self.types.substitute(self.ctx, *ty, &bindings);
                let concrete = self.instantiate_annotated_class(bound, 0, Some(s.inst))?;
                specialized |= concrete != *ty;
                *ty = concrete;
            }
            if specialized {
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
        Ok(self.types.generic(
            base,
            if is_map {
                &[Ty::OPEN, Ty::OPEN]
            } else {
                &[Ty::OPEN]
            },
        ))
    }

    pub(super) fn active_instance_type_bindings(
        &mut self,
        inst_id: InstanceId,
    ) -> Vec<(shapes::TypeId, Ty)> {
        let Some(inst) = self.instances.get(inst_id) else {
            return Vec::new();
        };
        let (template, receiver, capture_owner, args_ty, targs_ty) = (
            inst.template,
            inst.receiver,
            inst.capture_owner,
            inst.args,
            inst.explicit_type_args,
        );
        let args = self.types.tuple_slice(args_ty).to_vec();
        let explicit_targs = self.types.tuple_slice(targs_ty).to_vec();
        self.compute_type_param_bindings(template, receiver, capture_owner, &args, &explicit_targs)
    }

    pub(super) fn check_timer_member(&self, receiver: Ty, offset: u32) -> Result<()> {
        if matches!(self.types.kind(receiver), TyKind::Leaf(id) if crate::hir_compat::is_timer_handle(id.hir(self.ctx)))
        {
            let locations = &self.metadata.locations;
            let member = locations
                .member_text(offset)
                .ok_or_else(|| Diagnostic::internal("Timeout member has no source expression"))?;
            return Err(Diagnostic::unsupported(format!(
                "Timeout member '{member}' is not supported"
            ))
            .with_fallback_span(locations.member_span(offset)));
        }
        Ok(())
    }

    fn eval_property_get(
        &mut self,
        s: &mut EvalState<'h>,
        object: &'h Expr,
        property: &str,
        byte_offset: u32,
    ) -> Result<Ty> {
        if let Expr::ClassRef(class_name) = object {
            return Ok(self.read_static_field(s.inst, class_name, property));
        }
        let mut obj_ty = self.eval_expr(s, object)?;
        if obj_ty == Ty::BOTTOM {
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        self.check_timer_member(stripped, byte_offset)?;
        if matches!(property, "payload" | "val") && stripped == self.types.named(self.ctx, "Error")
        {
            return Err(Diagnostic::unsupported(format!(
                "property {property} is not supported on Error yet"
            ))
            .with_fallback_span(
                self.metadata
                    .locations
                    .member_span(byte_offset)
                    .or_else(|| {
                        self.metadata
                            .locations
                            .offset_span(s.iteration.object_pattern_offset(object)?, 1)
                    }),
            ));
        }
        if let TyKind::Cell {
            loc,
            kind: CellKind::Dict,
        } = self.types.kind(stripped)
        {
            if self.dict_reads.insert(loc.index()) {
                self.dirty_locs.insert(loc.index());
            }
            obj_ty = self.resolve_cell_read(s.inst, obj_ty);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        if let Some(cinst_id) = self.resolve_class_inst(s.inst, stripped, 0)? {
            if let Some(loc) = self.class_field_loc(cinst_id, property) {
                return Ok(self.read_loc(loc, s.inst));
            }
            let pname = self.types.intern_name(property);
            if let Some(cinst) = self.class_insts.get(cinst_id)
                && let Some(getter_tid) = self
                    .index
                    .class(cinst.class)
                    .and_then(|c| ClassInfo::find(&c.getters, pname))
            {
                let target = self.request_instance(
                    InstanceReq::new(getter_tid, &[], Some(s.inst))
                        .receiver(Some(Receiver::Class(cinst_id))),
                )?;
                return Ok(self.read_loc(self.instances[target].return_loc, s.inst));
            }
        }
        Ok(
            builtins::property_type(self.ctx, self.decls, self.types, obj_ty, property)
                .unwrap_or(Ty::OPEN),
        )
    }

    pub(super) fn class_field_loc(&self, cinst_id: ClassInstId, property: &str) -> Option<LocId> {
        let cinst = self.class_insts.get(cinst_id)?;
        let f_idx = self
            .hir
            .classes
            .get(self.index.class(cinst.class)?.hir_idx)?
            .fields
            .iter()
            .position(|f| f.name == property)?;
        self.field_locs
            .get(cinst.field_locs.range())
            .and_then(|sl| sl.get(f_idx))
            .copied()
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
                self.check_escaping_func(s.inst, val_ty, 0, s.current_span)?;
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
