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

    pub(super) fn check_objlit_func_write(
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
                    && self.index.view(tid).kind == TemplateKind::Closure
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

    pub(super) fn write_class_or_setter(
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
            && let Some(setter_tid) = ClassInfo::find(
                &self.index.class(self.class_insts[cinst_id].class).setters,
                prop_id,
            )
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

    pub(super) fn eval_property_get(
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
            if let Some(getter_tid) = ClassInfo::find(
                &self.index.class(self.class_insts[cinst_id].class).getters,
                pname,
            ) {
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
        let cinst = &self.class_insts[cinst_id];
        let f_idx = self
            .hir
            .classes
            .get(self.index.class(cinst.class).hir_idx)?
            .fields
            .iter()
            .position(|f| f.name == property)?;
        self.field_locs[cinst.field_locs].get(f_idx).copied()
    }

    pub(super) fn object_props_of(&mut self, stripped: Ty) -> Option<Vec<ObjectProp>> {
        if let TyKind::Object { props, .. } = self.types.kind(stripped) {
            return Some(self.types.object_slice(props).to_vec());
        }
        if matches!(
            self.types.kind(stripped),
            TyKind::Leaf(_) | TyKind::Generic { .. }
        ) {
            let hir = self.types.materialize_hir(self.ctx, stripped);
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
