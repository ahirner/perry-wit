//! Method call resolution on classes, resources, function properties, and built-ins.
use super::*;
use crate::methods::MethodResult;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn method_result(
        &mut self,
        inst: InstanceId,
        rule: MethodResult,
        mut call: MethodResultArgs<'_>,
    ) -> Ty {
        if rule == MethodResult::Receiver && self.arrays.contains(self.types, call.receiver) {
            return call.receiver;
        }
        if !matches!(
            rule,
            MethodResult::Number
                | MethodResult::Boolean
                | MethodResult::String
                | MethodResult::Void
        ) {
            call.receiver = self.resolve_cell_read(inst, call.receiver);
        }
        if call.receiver == Ty::BOTTOM {
            return Ty::BOTTOM;
        }
        builtins::apply_method_result(self.ctx, self.types, rule, call)
    }

    pub(super) fn method_property(
        &mut self,
        s: &mut EvalState<'h>,
        stripped: Ty,
        name: &str,
        byte_offset: u32,
    ) -> Result<Option<Ty>> {
        Ok(
            if let Some(cinst_id) = self.resolve_class_inst(s.inst, stripped, byte_offset)?
                && let Some(loc) = self.class_field_loc(cinst_id, name)
            {
                Some(self.read_loc(loc, s.inst))
            } else {
                builtins::property_type(self.ctx, self.decls, self.types, stripped, name)
            },
        )
    }

    pub(super) fn eval_method_call(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        object: &'h Expr,
        property: &str,
        args: &'h [Expr],
        byte_offset: u32,
    ) -> Result<Ty> {
        let method = hir_compat::method(property);
        let name = method.name();

        if matches!(object, Expr::GlobalGet(_)) {
            match name {
                "log" | "info" | "debug" | "error" | "warn"
                    if matches!(object, Expr::GlobalGet(0)) =>
                {
                    self.eval_args(s, args)?;
                    return Ok(Ty::VOID);
                }
                "fetch" => {
                    self.eval_args(s, args)?;
                    let resp = self.types.named(self.ctx, "Response");
                    return Ok(self.types.promise(resp));
                }
                "queueMicrotask" => {
                    return Err(self.annotate_expr_error(
                        s,
                        Diagnostic::unsupported(
                            "queueMicrotask is not supported: this runtime has no microtask queue",
                        ),
                        byte_offset,
                    ));
                }
                "setTimeout" | "setInterval" | "setImmediate" => {
                    return self.eval_timer_start(s, expr, name, args, byte_offset);
                }
                "clearTimeout" | "clearInterval" | "clearImmediate" => {
                    return self.eval_timer_clear(s, expr, args);
                }
                _ => {}
            }
        }
        if name == "toWeb" && let Expr::ExternFuncRef { name: local, .. } = object
            && self.hir.declarations.imports.iter().any(|imp| matches!(imp.source.as_str(), "stream" | "node:stream")
                && imp.specifiers.iter().any(|sp| matches!(sp, perry_hir::ir::ImportSpecifier::Named { imported, local: l } if imported == "Writable" && l == local)))
        {
            self.eval_args(s, args)?;
            return Ok(self.types.named(self.ctx, "WritableStream"));
        }
        if let Expr::PropertyGet {
            object: global,
            property: class,
            ..
        } = object
            && matches!(global.as_ref(), Expr::GlobalGet(_))
        {
            let mut arg_tys = self.eval_args(s, args)?;
            if class == "Promise" {
                for ty in &mut arg_tys {
                    *ty = self.resolve_promise_cells(s.inst, *ty);
                }
            }
            return Ok(builtins::static_global_method(
                self.ctx, self.types, class, name, args, &arg_tys,
            )
            .unwrap_or(Ty::OPEN));
        }
        if let Expr::PropertyGet {
            object: ns,
            property: class,
            ..
        } = object
            && let Expr::PropertyGet {
                object: global,
                property: ns_name,
                ..
            } = ns.as_ref()
            && matches!(global.as_ref(), Expr::GlobalGet(_))
            && ns_name == "Temporal"
        {
            self.eval_args(s, args)?;
            return Ok(
                builtins::temporal_static_method(self.ctx, self.types, class, property)
                    .unwrap_or(Ty::OPEN),
            );
        }
        if let Expr::ClassRef(class_name) = object {
            return self.eval_static_method_call(s, expr, class_name, name, args, byte_offset);
        }

        let obj_ty = self.eval_expr(s, object)?;
        if obj_ty == Ty::BOTTOM {
            self.eval_args(s, args)?;
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);

        if let TyKind::Named(id) = self.types.kind(stripped)
            && let Some(owner) = self.decls.resource(id.get(self.ctx))
        {
            self.eval_args(s, args)?;
            if matches!(method, hir_compat::Method::UsingCheck)
                && let Expr::LocalGet(id) = object
            {
                let span = self
                    .hir
                    .declarations
                    .locals
                    .get(id)
                    .and_then(|s| self.hir.locations.span(s.span));
                return Err(Diagnostic::unsupported(
                    "using declarations for imported resources are unsupported",
                )
                .with_fallback_span(span));
            }
            if let Some(member) = self.index.receiver_members.get(object, owner) {
                self.record_site(
                    s.inst,
                    expr,
                    SiteResolution::MemberCall(MemberTarget::Resource(member)),
                );
                return Ok(self.resource_result(member));
            }
            return Err(self.annotate_expr_error(
                s,
                Diagnostic::unsupported(format!(
                    "method {name} is not supported on WIT resource yet"
                )),
                byte_offset,
            ));
        }
        self.check_timer_member(stripped, byte_offset)?;

        if matches!(name, "then" | "catch" | "finally")
            && matches!(
                self.types.kind(stripped),
                TyKind::Promise(_)
                    | TyKind::Cell {
                        kind: CellKind::Promise,
                        ..
                    }
            )
        {
            return self.eval_promise_reaction(s, expr, stripped, name, args, byte_offset);
        }

        let arg_tys = self.eval_args(s, args)?;

        if name == "concat"
            && self.type_may_be_array(stripped)
            && self.arrays.contains(self.types, stripped)
        {
            for (value, &ty) in args.iter().zip(&arg_tys) {
                self.push_argument(s.inst, stripped, value, ty, self.type_may_be_array(ty));
            }
            return Ok(stripped);
        }
        if name == "set"
            && arg_tys.get(1).is_some_and(|&ty| self.types.has_func(ty))
            && builtins::receiver_of(self.ctx, self.types, stripped)
                == Some(crate::methods::Receiver::Map)
        {
            return Err(self.annotate_expr_error(
                s,
                Diagnostic::unsupported("storing a function value with Map.set is not supported"),
                byte_offset,
            ));
        }
        if matches!(name, "push" | "unshift") && self.push_values(s.inst, stripped, args, &arg_tys)
        {
            return Ok(Ty::NUMBER);
        }

        if let Some(cinst_id) = self.resolve_class_inst(s.inst, stripped, byte_offset)?
            && let Some(method_tid) = self
                .index
                .method_template(&self.class_declaration(cinst_id).methods, name)
        {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let view = self.index.view(method_tid);
            self.check_callback_params(s.inst, view, &arg_tys, byte_offset)?;
            let (_, ret) = self.dispatch_call_site(
                s.inst,
                expr,
                InstanceReq::new(method_tid, &arg_tys, Some(s.inst))
                    .receiver(Some(Receiver::Class(cinst_id)))
                    .at(byte_offset),
                Some(args),
                false,
            )?;
            return Ok(ret);
        }

        if let Some(ty) = self.method_property(s, stripped, name, byte_offset)?
            && self.types.has_func(ty)
        {
            return self.eval_callable(s, expr, ty, Some(stripped), &arg_tys);
        }

        if let Some(rule) =
            builtins::lookup_method_result(self.ctx, self.types, stripped, name, false)
        {
            return Ok(self.method_result(
                s.inst,
                rule,
                MethodResultArgs {
                    receiver: obj_ty,
                    arg0: arg_tys.first().copied(),
                    arg0_expr: args.first(),
                    callback_result: None,
                    sparse_arrays: self.hir.sparse_arrays,
                },
            ));
        }
        let is_known_js_cb_method = matches!(
            name,
            "map"
                | "flatMap"
                | "filter"
                | "forEach"
                | "reduce"
                | "reduceRight"
                | "some"
                | "every"
                | "find"
                | "findIndex"
                | "sort"
                | "toSorted"
                | "replace"
                | "replaceAll"
                | "then"
                | "catch"
                | "finally"
                | "groupBy"
        );
        let is_collection = matches!(
            self.types.kind(stripped),
            TyKind::Array(_) | TyKind::Cell { .. }
        ) || matches!(self.types.kind(stripped), TyKind::Generic { base, .. }
            if matches!(base.get(self.ctx).as_str(), "Map" | "Set" | "ReadonlyMap" | "ReadonlySet"));
        let skip_host_cb = is_collection || (self.types.is_open(stripped) && is_known_js_cb_method);
        if hir_compat::has_source_offset(byte_offset)
            && !skip_host_cb
            && arg_tys.iter().any(|&a| self.types.has_func(a))
        {
            return Err(self.hir.locations.annotate_call(
                Diagnostic::unsupported(format!(
                    "passing a callback to runtime '{name}' requires the runtime to call a guest function"
                )),
                byte_offset,
            ));
        }
        Ok(Ty::OPEN)
    }
}
