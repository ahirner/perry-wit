//! Property access, class fields, object spreads, collections, and function comparisons.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn eval_children(
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn assign_property(
        &mut self,
        s: &EvalState<'h>,
        site: &'h Expr,
        obj_def: Ty,
        property: &str,
        value: &'h Expr,
        val_ty: Ty,
        byte_offset: u32,
    ) -> Result<()> {
        self.check_escaping_func(s.inst, val_ty, byte_offset, s.current_span)?;
        let field_ty = match self.types.kind(obj_def) {
            TyKind::Object { .. } => {
                let declared =
                    builtins::property_type(self.ctx, self.decls, self.types, obj_def, property);
                let touches_func =
                    self.types.has_func(val_ty) || declared.is_some_and(|p| self.types.has_func(p));
                if touches_func
                    && val_ty != Ty::BOTTOM
                    && !declared.is_some_and(|p| self.types.accepts(p, val_ty))
                {
                    let mut err = Diagnostic::unsupported(format!(
                        "reassigning function property '{property}' on an object literal is not supported yet"
                    ));
                    if hir_compat::has_source_offset(byte_offset) {
                        err = err.with_fallback_span(self.hir.locations.member_span(byte_offset));
                    }
                    return Err(self.annotate_expr_error(s, err, byte_offset));
                }
                declared.unwrap_or(val_ty)
            }
            TyKind::ClassInst(class) => {
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                if let Some(loc) = self.class_field_loc(class, property) {
                    self.write_class_field(s.inst, loc, value, norm)?
                } else if norm != Ty::BOTTOM
                    && let Some(setter) =
                        self.request_accessor(s.inst, class, property, Some(norm))?
                {
                    self.record_site(s.inst, site, SiteResolution::WriteMember(setter));
                    return Ok(());
                } else {
                    val_ty
                }
            }
            _ => val_ty,
        };
        if self
            .types
            .is_zero_sized_func(field_ty, &self.index.capture_free_templates)
        {
            self.record_site(s.inst, site, SiteResolution::ZeroSizedFieldSet);
        } else {
            self.clear_site(s.inst, site);
            self.record_coercion(s.inst, value, val_ty, field_ty);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_func_compare(
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
        self.mark_escaping_env(l);
        self.mark_escaping_env(r);
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
                    && matches!(self.index.view(tid).kind, TemplateKind::Closure(_))
                    && (self.index.closure_in_loop.contains(tid.index())
                        || self
                            .index
                            .closure_owner(tid)
                            .is_some_and(|owner| owner != TemplateId::INIT))
                {
                    let err = Diagnostic::unsupported(
                        "cannot compare a capture-free closure literal that may be evaluated more than once",
                    );
                    let err = match span {
                        Some(sp) => self.hir.locations.annotate(err, sp),
                        None => err,
                    };
                    return Err(attach_inst_chain(
                        &self.instances,
                        self.index,
                        &self.hir.locations,
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
        let erased = |ty| match self.types.kind(ty) {
            TyKind::Func(id)
                if self
                    .types
                    .is_zero_sized_func(ty, &self.index.capture_free_templates) =>
            {
                Some(id)
            }
            _ => None,
        };
        let comparison = if let Some((identity, source, left_erased)) = erased(l)
            .map(|id| (id, r, true))
            .or_else(|| erased(r).map(|id| (id, l, false)))
        {
            if self
                .types
                .func_members(&source)
                .any(|member| member == identity)
            {
                SiteResolution::Matches {
                    identity,
                    source,
                    left_erased,
                    equal: is_eq,
                }
            } else {
                SiteResolution::FoldBool(!is_eq)
            }
        } else {
            SiteResolution::FuncCompare {
                left_ty: l,
                right_ty: r,
                equal: is_eq,
                environments: l == r
                    && matches!(self.types.kind(l), TyKind::Func(id) if self.is_escaping_env(id.template, inst_id)),
            }
        };
        self.record_site(inst_id, expr, comparison);
        Ok(())
    }

    pub(super) fn write_class_field(
        &mut self,
        inst_id: InstanceId,
        loc: LocId,
        value: &Expr,
        norm: Ty,
    ) -> Result<Ty> {
        let existing = self.facts[loc];
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
        Ok(self.read_loc(loc, inst_id))
    }

    pub(super) fn request_accessor(
        &mut self,
        caller: InstanceId,
        receiver: ClassInstId,
        property: &str,
        assigned: Option<Ty>,
    ) -> Result<Option<InstanceId>> {
        let class = self.class_declaration(receiver);
        let accessors = if assigned.is_some() {
            &class.setters
        } else {
            &class.getters
        };
        let Some((_, member)) = accessors.iter().find(|(name, _)| name == property) else {
            return Ok(None);
        };
        let template = self
            .index
            .template_of_func(member.id)
            .ok_or_else(|| Diagnostic::internal("missing accessor declaration"))?;
        self.request_instance(
            InstanceReq::new(template, assigned.as_slice(), Some(caller))
                .receiver(Some(Receiver::Class(receiver))),
        )
        .map(Some)
    }

    pub(super) fn eval_local_get(&mut self, s: &mut EvalState<'h>, id: LocalId) -> Result<Ty> {
        let Some(lidx) = self.index.local_idx(id) else {
            return Ok(Ty::OPEN);
        };
        let local = self.index.locals[lidx];
        let Some(mut ty) = self.read_local_value(s.inst, lidx) else {
            return Ok(local.declared);
        };
        if local.flags.contains(LocalFlags::DEFERRED)
            && !self.inst_written[s.inst].contains(lidx.index())
            && ty == Ty::BOTTOM
        {
            return Ok(Ty::VOID);
        }
        if !local.flags.contains(LocalFlags::WRITTEN_FROM_CLOSURE)
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
        constructor: crate::source::Constructor<'h>,
    ) -> Result<Ty> {
        let is_map = constructor.kind == crate::source::CollectionKind::Map;
        let init = constructor.init;
        if let Some(init) = init {
            self.eval_expr(s, init)?;
        }
        if let Some(SiteResolution::CollectionNew(declared)) = self.index.expressions.declared(expr)
        {
            let bound = self
                .index
                .substitute(self.types, declared, &self.type_bindings[s.inst]);
            let ty = self.instantiate_annotated_class(bound, 0, Some(s.inst))?;
            self.record_site(s.inst, expr, SiteResolution::CollectionNew(ty));
            return Ok(ty);
        }
        let base = shapes::TypeName::new(self.ctx, constructor.kind.name().into());
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

    pub(super) fn check_timer_member(&self, receiver: Ty, offset: u32) -> Result<()> {
        if receiver == Ty::TIMER {
            let locations = &self.hir.locations;
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

    pub(super) fn eval_property_get(
        &mut self,
        s: &mut EvalState<'h>,
        site: &'h Expr,
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
            .with_fallback_span(self.hir.locations.member_span(byte_offset).or_else(|| {
                self.hir
                    .locations
                    .offset_span(self.index.iteration.object_pattern_offset(object)?, 1)
            })));
        }
        if let TyKind::Cell {
            loc,
            kind: CellKind::Dict,
        } = self.types.kind(stripped)
        {
            if self.dict_reads.insert(loc.index()) {
                self.facts.dirty.insert(Fact::Storage(loc));
            }
            obj_ty = self.resolve_cell_read(s.inst, obj_ty);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        if let Some(cinst_id) = self.resolve_class_inst(s.inst, stripped, 0)? {
            if let Some(loc) = self.class_field_loc(cinst_id, property) {
                return Ok(self.read_loc(loc, s.inst));
            }
            if let Some(target) = self.request_accessor(s.inst, cinst_id, property, None)? {
                self.record_site(s.inst, site, SiteResolution::ReadMember(target));
                return Ok(self.read_loc(self.instances[target].return_loc, s.inst));
            }
        }
        Ok(
            builtins::property_type(self.ctx, self.decls, self.types, obj_ty, property)
                .unwrap_or(Ty::OPEN),
        )
    }

    pub(super) fn class_declaration(&self, id: ClassInstId) -> &'h perry_hir::ir::Class {
        self.index.class(self.class_insts[id].class).declaration
    }

    pub(super) fn class_field_loc(&self, cinst_id: ClassInstId, property: &str) -> Option<LocId> {
        let cinst = &self.class_insts[cinst_id];
        let f_idx = self
            .class_declaration(cinst_id)
            .fields
            .iter()
            .position(|f| f.name == property)?;
        self.field_locs[cinst.field_locs].get(f_idx).copied()
    }

    pub(super) fn object_props_of(&mut self, stripped: Ty) -> Option<Vec<ObjectProp>> {
        if let TyKind::Object { props, .. } = self.types.kind(stripped) {
            return Some(self.types.object_slice(props).to_vec());
        }
        let props = crate::shapes::generics::Resolver::new(self.ctx, self.decls, self.types)
            .record(stripped, 0)?;
        let mut props = self.types.object_slice(props).to_vec();
        for p in &mut props {
            if p.optional {
                p.ty = self.types.with_absences(p.ty, Absence::Undefined);
            }
        }
        Some(props)
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
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                let field = self.types.property(key, norm, false);
                if let Some(ex) = props.iter_mut().find(|p| p.name == field.name) {
                    ex.ty = match val_expr {
                        Expr::Null => self.types.with_absences(ex.ty, Absence::Null),
                        Expr::Undefined => self.types.with_absences(ex.ty, Absence::Undefined),
                        _ => norm,
                    };
                } else {
                    props.push(field);
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
