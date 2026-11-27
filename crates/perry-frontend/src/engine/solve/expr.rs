//! Expression, property, collection-constructor, and object-spread evaluation.
use super::*;
use crate::methods::MethodResult;
use crate::source::ConstructorSites;

impl<'h, 'a> Solver<'h, 'a> {
    fn static_field_loc(&self, class: &str, field: &str) -> Option<LocId> {
        self.index
            .class(self.index.class_named(class)?)
            .static_field(field)
    }

    pub(super) fn read_static_field(&mut self, reader: InstanceId, class: &str, field: &str) -> Ty {
        self.static_field_loc(class, field)
            .map_or(Ty::OPEN, |loc| self.read_loc(loc, reader))
    }

    pub(super) fn eval_expr(&mut self, s: &mut EvalState<'h>, expr: &'h Expr) -> Result<Ty> {
        if let Some(record) = self.index.iteration.expression(expr) {
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
        let declared = self.index.expressions.declared(expr);
        if let Some(SiteResolution::Regex(literal)) = declared {
            for arg in self
                .index
                .expressions
                .regexp(expr)
                .into_iter()
                .flat_map(|args| args.operands())
            {
                self.eval_expr(s, arg)?;
            }
            let shape = literal
                .map(|literal| literal.shape(self.ctx))
                .transpose()
                .map_err(|error| {
                    let template = self.index.view(self.instances[s.inst].template);
                    let locations = &self.hir.locations;
                    let error = error.with_fallback_span(locations.function_span(&template.name));
                    attach_inst_chain(&self.instances, self.index, locations, error, s.inst)
                })?
                .unwrap_or_default();
            return Ok(self.types.regex(shape));
        }
        if let Some(constructor) = self.index.expressions.collection(expr) {
            return self.eval_collection_constructor(s, expr, constructor);
        }
        if let Some(decision) = declared
            && let Some((args, _)) = hir_compat::call_arguments(expr)
        {
            match decision {
                SiteResolution::Static(method) => return self.eval_static_call(s, method, args),
                SiteResolution::ResourceCall(id) => {
                    let types = self.eval_args(s, args)?;
                    return Ok(
                        if types.contains(&Ty::BOTTOM)
                            && matches!(expr, Expr::StaticMethodCall { .. })
                        {
                            Ty::BOTTOM
                        } else {
                            self.resource_result(id)
                        },
                    );
                }
                _ => {}
            }
        }
        if let Some(cb_ty) = self.try_eval_callback_intrinsic(s, expr)? {
            return Ok(cb_ty);
        }

        if let Some(call) = SourceCall::of(expr) {
            return self.eval_call(s, call);
        }
        if let Some((object, key)) = hir_compat::member(expr) {
            return match key {
                hir_compat::MemberKey::Named(name, offset) => {
                    self.eval_property_get(s, expr, object, name, offset)
                }
                hir_compat::MemberKey::Computed(index, offset) => {
                    let receiver = self.eval_expr(s, object)?;
                    self.eval_index(s, expr, receiver, index, offset)
                }
            };
        }
        if let Some((object, key, value)) = hir_compat::member_assignment(expr) {
            let receiver = self.eval_expr(s, object)?;
            let receiver = self.types.strip_nullish(receiver).0;
            let key_type = match key {
                hir_compat::MemberKey::Named(..) => Ty::STRING,
                hir_compat::MemberKey::Computed(index, _) => self.eval_expr(s, index)?,
            };
            let ty = self.eval_expr(s, value)?;
            if let hir_compat::MemberKey::Named(name, offset) = key {
                self.assign_property(s, expr, receiver, name, value, ty, offset)?;
            }
            self.write_cell_indexed(receiver, key_type, ty);
            self.coerce_array_write(s.inst, receiver, std::slice::from_ref(value), &[ty]);
            return Ok(ty);
        }
        match expr {
            Expr::LocalGet(id) => self.eval_local_get(s, *id),
            Expr::LocalSet(id, value) => {
                if self.index.iteration.is_release(expr) {
                    return Ok(Ty::VOID);
                }
                s.guards.assign(*id);
                let val_ty = self.eval_expr(s, value)?;
                if let Some(lidx) = self.index.local_idx(*id) {
                    if let Some(existing) = self.read_local_value(s.inst, lidx) {
                        let is_inferred = !self.index.locals[lidx].flags.is_explicit();
                        let local_span = self.hir.declarations.locals.get(id).map(|sp| sp.span);
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
                            self.write_local_value(s, lidx, val_ty)?;
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
                            && self.index.locals[lidx]
                                .flags
                                .intersects(LocalFlags::DEFERRED_OR_NULLISH))
                            || self.types.is_open(existing)
                            || matches!(self.types.kind(existing), TyKind::Regex(_))
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
                            self.write_local_value(s, lidx, norm)?;
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
                    if self.class_declaration(id).id == *class_id)
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
            Expr::Closure { func_id, .. } => {
                let Some(template) = self.index.template_of_func(*func_id) else {
                    return Ok(Ty::OPEN);
                };
                for &lid in self.index.closure_references(template) {
                    if let Some(ty) = self
                        .index
                        .local_idx(lid)
                        .and_then(|l| self.read_local_value(s.inst, l))
                    {
                        self.invalidate_conservative_cell(ty);
                    }
                }
                let identity = self.closure_identity(s.inst, template);
                let ty = self.types.func(identity);
                if self.index.view(template).captures_this && s.receiver.is_some() {
                    self.mark_escaping_env(ty);
                }
                self.record_site(s.inst, expr, SiteResolution::Closure(identity));
                if self.is_escaping_env(template, s.inst) {
                    for &cap_id in self.index.template_captures(template) {
                        if let Some(lidx) = self.index.local_idx(cap_id)
                            && let Some(cap_ty) = self.read_local_value(s.inst, lidx)
                        {
                            self.mark_escaping_env(cap_ty);
                        }
                    }
                }
                Ok(ty)
            }
            Expr::Binary { op, left, right } => {
                let (l, r) = (self.eval_expr(s, left)?, self.eval_expr(s, right)?);
                Ok(builtins::binary_result(self.types, *op, l, r))
            }
            Expr::Unary { op, operand } => {
                Ok(builtins::unary_result(*op, self.eval_expr(s, operand)?))
            }
            Expr::Logical { op, left, right } => {
                let l = self.eval_expr(s, left)?;
                let depth = s.guards.depth();
                if matches!(op, LogicalOp::And | LogicalOp::Or) {
                    self.assume(s, left, *op == LogicalOp::And);
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
                self.assume(s, condition, true);
                let mut then_ty = self.eval_expr(s, then_expr)?;
                if guard.as_ref().is_some_and(|g| g.proves(true, then_expr)) {
                    then_ty = self.types.exclude_absences(then_ty, Absence::Undefined);
                }
                let then_writes = s.guards.other_arm(branch);
                self.assume(s, condition, false);
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
                if let Some(span) = self.hir.locations.operator_raw_span(parts)
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
                    self.write_local_value(s, lidx, val_ty)?;
                    self.mark_written(s.inst, lidx);
                }
                self.eval_expr(s, body)
            }
            Expr::Await(promise) => {
                let ty = self.eval_expr(s, promise)?;
                let ty = self.resolve_cell_read(s.inst, ty);
                Ok(self.awaited_type(s.inst, ty))
            }
            Expr::QueueMicrotask(_) | Expr::ProcessNextTick { .. } => {
                let api = if matches!(expr, Expr::QueueMicrotask(_)) {
                    "queueMicrotask"
                } else {
                    "process.nextTick"
                };
                Err(self.annotate_expr_error(
                    s,
                    Diagnostic::unsupported(format!(
                        "{api} is not supported: this runtime has no microtask queue"
                    )),
                    0,
                ))
            }
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
                if self.types.has_func(joined) {
                    let array = self.callable_array(s.inst, expr, joined)?;
                    self.coerce_array_write(s.inst, array, elements, &elem_tys);
                    return Ok(array);
                }
                Ok(self.types.array(joined))
            }
            Expr::ArraySpread(items) => {
                let (mut joined, mut has_bottom) = (Ty::BOTTOM, false);
                let mut functions = Vec::new();
                for item in items {
                    let t = match item {
                        perry_hir::ir::ArrayElement::Expr(e) => {
                            let v = self.eval_expr(s, e)?;
                            if self.types.has_func(v) {
                                functions.push((e, v, false));
                            }
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
                            let input = self.eval_expr(s, e)?;
                            let src = self.resolve_cell_read(s.inst, input);
                            if src == Ty::BOTTOM {
                                has_bottom = true;
                                Ty::BOTTOM
                            } else {
                                let element = builtins::iterated_element(self.ctx, self.types, src)
                                    .unwrap_or(Ty::OPEN);
                                if self.types.has_func(element) {
                                    functions.push((e, input, true));
                                }
                                element
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
                if self.types.has_func(joined) {
                    let array = self.callable_array(s.inst, expr, joined)?;
                    for (value, ty, spread) in functions {
                        self.push_argument(s.inst, array, value, ty, spread);
                    }
                    return Ok(array);
                }
                let array = self.types.array(joined);
                self.record_site(s.inst, expr, SiteResolution::TypedArray(array));
                Ok(array)
            }
            Expr::ArrayFrom(source, _) => {
                let input = self.eval_expr(s, source)?;
                let src = self.resolve_cell_read(s.inst, input);
                if src == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                if matches!(source.as_ref(), Expr::Object(p) if p.iter().any(|(k, _)| k == "length"))
                {
                    return Ok(self.types.array(Ty::OPEN));
                }
                Ok(
                    match builtins::iterated_element(self.ctx, self.types, src) {
                        Some(element) if self.types.has_func(element) => {
                            let array = self.callable_array(s.inst, expr, element)?;
                            self.share_element_layouts(&[array, input]);
                            array
                        }
                        Some(element) => self.types.array(element),
                        None => Ty::OPEN,
                    },
                )
            }
            Expr::Object(fields) => {
                self.eval_object_fields(s, fields.iter().map(|(n, v)| (n.as_str(), v)))
            }
            Expr::ObjectSpread { parts } => {
                let ty = self.eval_object_spread(s, parts)?;
                self.record_site(s.inst, expr, SiteResolution::ObjectSpread(ty));
                Ok(ty)
            }
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
            Expr::StringCoerce(object) | Expr::TemplateStringCoerce(object) => {
                let ty = self.eval_expr(s, object)?;
                let receiver = self.types.strip_nullish(ty).0;
                if let Some(class) = self.resolve_class_inst(s.inst, receiver, 0)?
                    && let Some(template) = self
                        .index
                        .method_template(&self.class_declaration(class).methods, "toString")
                {
                    let target = self.request_instance(
                        InstanceReq::new(template, &[], Some(s.inst))
                            .receiver(Some(Receiver::Class(class))),
                    )?;
                    self.record_site(s.inst, expr, SiteResolution::ReadMember(target));
                }
                Ok(Ty::STRING)
            }
            Expr::PropertyUpdate {
                object,
                property,
                byte_offset,
                ..
            } => {
                let ty = self.eval_expr(s, object)?;
                let receiver = self.types.strip_nullish(ty).0;
                if let Some(class) = self.resolve_class_inst(s.inst, receiver, *byte_offset)?
                    && self.class_field_loc(class, property).is_none()
                {
                    let get = self.request_accessor(s.inst, class, property, None)?;
                    let set = self.request_accessor(s.inst, class, property, Some(Ty::NUMBER))?;
                    if get.is_some() || set.is_some() {
                        self.record_site(s.inst, expr, SiteResolution::UpdateMember { get, set });
                    }
                }
                Ok(Ty::NUMBER)
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
                if let Some(loc) = self.static_field_loc(class_name, field_name) {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, val_ty);
                    self.write_loc(loc, norm);
                }
                Ok(val_ty)
            }
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
                let name = match callee.as_ref() {
                    Expr::PropertyGet { property, .. } => property.as_str(),
                    Expr::ExternFuncRef { name, .. } => name.as_str(),
                    _ => "constructor",
                };
                self.check_host_arguments(s, name, &arg_tys, *byte_offset)?;
                Ok(Ty::OPEN)
            }
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
                        && self.push_values(s.inst, stripped, args, &arg_tys)
                    {
                        return Ok(Ty::NUMBER);
                    }
                    if let Some(rule) =
                        self.resolve_builtin(s, expr, stripped, method, args.is_empty())
                    {
                        return Ok(self.method_result(
                            s.inst,
                            rule.result(),
                            MethodResultArgs::call(obj_ty, args, &arg_tys, self.hir.sparse_arrays),
                        ));
                    }
                }
                self.check_host_arguments(s, method, &arg_tys, 0)?;
                Ok(Ty::OPEN)
            }
            Expr::ObjectValues(object) | Expr::ObjectEntries(object) => {
                let obj_ty = self.eval_expr(s, object)?;
                if obj_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let obj_ty = self.resolve_cell_read(s.inst, obj_ty);
                let stripped = self.types.strip_nullish(obj_ty).0;
                Ok(
                    crate::shapes::generics::Resolver::new(self.ctx, self.decls, self.types)
                        .dictionary(stripped)
                        .map_or(Ty::OPEN, |(k, v)| {
                            let elem = if matches!(expr, Expr::ObjectEntries(_)) {
                                self.types.tuple(&[k, v])
                            } else {
                                v
                            };
                            self.types.array(elem)
                        }),
                )
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
                        shapes::TypeName::new(self.ctx, "Record".into()),
                        &[Ty::STRING, val],
                    ));
                }
                Ok(Ty::OPEN)
            }
            Expr::CryptoRandomFillSync {
                buffer,
                offset,
                size,
            } => {
                let ty = self.eval_expr(s, buffer)?;
                self.eval_expr(s, offset)?;
                self.eval_expr(s, size)?;
                Ok(ty)
            }
            Expr::ArrayShift(id) => {
                let arr = self.eval_local_get(s, *id)?;
                Ok(self.method_result(
                    s.inst,
                    MethodResult::Element,
                    MethodResultArgs::receiver_only(arr),
                ))
            }
            Expr::TypeOf(operand) => {
                let op_ty = self.eval_expr(s, operand)?;
                if op_ty != Ty::BOTTOM && self.types.has_func(op_ty) {
                    let has_nullish = self.types.strip_nullish(op_ty).1.is_some();
                    self.record_site(
                        s.inst,
                        expr,
                        SiteResolution::FuncTypeOf(has_nullish.then_some(op_ty)),
                    );
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
            }
            | Expr::ArrayPushSpread {
                array_id,
                source: value,
            } => {
                let val_ty = self.eval_expr(s, value)?;
                if let Some(arr_ty) = self
                    .index
                    .local_idx(*array_id)
                    .and_then(|l| self.read_local_value(s.inst, l))
                {
                    let def = self.types.strip_nullish(arr_ty).0;
                    self.push_argument(
                        s.inst,
                        def,
                        value,
                        val_ty,
                        matches!(expr, Expr::ArrayPushSpread { .. }),
                    );
                }
                Ok(Ty::NUMBER)
            }
            Expr::Delete(target) => {
                if let Some((object, _)) = hir_compat::member(target) {
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
                    if let Some((first, second)) = call.array_callback()
                        && let Some(result) =
                            self.eval_callback_method(s, expr, ty, call.method(), first, second)?
                    {
                        return Ok(result);
                    }
                    self.eval_children(s, expr, evaluated)?;
                    return Ok(self.method_result(
                        s.inst,
                        call.method().result_rule(),
                        MethodResultArgs::receiver_only(ty),
                    ));
                }
                self.eval_children(s, expr, None)?;
                Ok(builtins::leaf_expr_type(self.ctx, self.types, expr))
            }
        }
    }
}
