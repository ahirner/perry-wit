//! Method call resolution on classes, resources, function properties, and built-ins.
use super::*;
use crate::methods::{Method, MethodResult};

#[derive(Clone, Copy)]
pub(super) enum Builtin {
    Method(Method),
    Inferred(MethodResult),
}

impl Builtin {
    pub(super) fn result(self) -> MethodResult {
        match self {
            Self::Method(m) => m.result_rule(),
            Self::Inferred(rule) => rule,
        }
    }
}

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn resolve_builtin(
        &mut self,
        s: &EvalState<'h>,
        expr: &'h Expr,
        receiver: Ty,
        name: &str,
        allow_property: bool,
    ) -> Option<Builtin> {
        let Some(family) = builtins::receiver_of(self.ctx, self.decls, self.types, receiver) else {
            return match self.types.kind(receiver) {
                TyKind::ClassInst(_) => None,
                TyKind::Tuple(_) => Method::lookup(
                    crate::methods::Receiver::Value(crate::methods::Kind::Array),
                    name,
                )
                .filter(|m| !m.is_property() || allow_property)
                .map(Method::result_rule),
                _ => Method::unique_result_rule(name),
            }
            .map(Builtin::Inferred);
        };
        let method = Method::lookup(family, name).filter(|m| !m.is_property() || allow_property)?;
        self.record_site(s.inst, expr, SiteResolution::Builtin(method));
        Some(Builtin::Method(method))
    }

    pub(super) fn method_result(
        &mut self,
        inst: InstanceId,
        rule: MethodResult,
        mut call: MethodResultArgs<'_>,
    ) -> Ty {
        if rule == MethodResult::Receiver && self.arrays.contains(self.types, call.receiver) {
            return call.receiver;
        }
        use perry_mir::SignatureType::{Bool, Number, String};
        if !matches!(rule, MethodResult::Signature([] | [Number | Bool | String])) {
            call.receiver = self.resolve_cell_read(inst, call.receiver);
        }
        if call.receiver == Ty::BOTTOM {
            return Ty::BOTTOM;
        }
        builtins::apply_method_result(self.ctx, self.types, rule, call)
    }

    pub(super) fn search_pattern(&mut self, instance: InstanceId, expr: &Expr, ty: Ty) -> TyKind {
        let (present, _) = self.types.strip_nullish(ty);
        let kind = self.types.kind(present);
        if let TyKind::Regex(shape) = kind {
            self.record_operand_use(instance, expr, OperandUse::RegexSearch(shape));
        }
        kind
    }

    pub(super) fn eval_method_call(
        &mut self,
        s: &mut EvalState<'h>,
        call: SourceCall<'h>,
        callee: &'h Expr,
        object: &'h Expr,
        property: &str,
    ) -> Result<Ty> {
        let SourceCall {
            site: expr,
            offset: byte_offset,
            ..
        } = call;
        let method = hir_compat::method(property);
        let name = method.name();

        if let Some(args) = call.args.plain() {
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
                    _ => {}
                }
            }
            if name == "toWeb"
                && let Expr::ExternFuncRef { name: local, .. } = object
                && matches!(
                    self.decls.imports.get(local.as_str()),
                    Some(crate::shapes::Imported::Writable)
                )
            {
                self.eval_args(s, args)?;
                return Ok(self.types.named(self.ctx, "WritableStream"));
            }
        }
        let obj_ty = self.eval_expr(s, object)?;
        if obj_ty == Ty::BOTTOM {
            self.call_argument_types(s, call.args)?;
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);

        if let Some(args) = call.args.plain()
            && let TyKind::Named(id) = self.types.kind(stripped)
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
                self.record_site(s.inst, expr, SiteResolution::ResourceCall(member));
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

        if let Some(args) = call.args.plain()
            && matches!(name, "then" | "catch" | "finally")
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

        if call.args.plain().is_none() && name == "push" && self.type_may_be_array(stripped) {
            for (value, spread) in call.args {
                let ty = self.eval_expr(s, value)?;
                self.push_argument(s.inst, stripped, value, ty, spread);
            }
            return Ok(Ty::NUMBER);
        }
        let builtin = call
            .args
            .plain()
            .and_then(|_| self.resolve_builtin(s, expr, stripped, name, false));
        if let Some(Builtin::Method(method)) = builtin
            && let Some((first, rest)) = call.args.plain().and_then(|args| args.split_first())
            && let Some(result) =
                self.eval_callback_method(s, expr, obj_ty, method, first, rest.first())?
        {
            return Ok(result);
        }
        let arg_tys = self.call_argument_types(s, call.args)?;
        if let Some(args) = call.args.plain() {
            if name == "concat"
                && self.type_may_be_array(stripped)
                && self.arrays.contains(self.types, stripped)
            {
                for (value, &ty) in args.iter().zip(&arg_tys) {
                    self.push_argument(s.inst, stripped, value, ty, self.type_may_be_array(ty));
                }
                return Ok(stripped);
            }
            if matches!(builtin, Some(Builtin::Method(Method::MapSet)))
                && arg_tys.get(1).is_some_and(|&ty| self.types.has_func(ty))
            {
                return Err(self.annotate_expr_error(
                    s,
                    Diagnostic::unsupported(
                        "storing a function value with Map.set is not supported",
                    ),
                    byte_offset,
                ));
            }
            if matches!(name, "push" | "unshift")
                && self.push_values(s.inst, stripped, args, &arg_tys)
            {
                return Ok(Ty::NUMBER);
            }
        }
        let class = self.resolve_class_inst(s.inst, stripped, byte_offset)?;
        if let Some(cinst_id) = class
            && let Some(method_tid) = self
                .index
                .method_template(&self.class_declaration(cinst_id).methods, name)
        {
            return self.class_call(s.inst, call, method_tid, Some(cinst_id), &arg_tys);
        }

        if let Some(ty) = self.member_type(s.inst, callee, stripped, name, class)?
            && self.types.has_func(ty)
        {
            return self.eval_callable(s, call, ty, Some(stripped), &arg_tys);
        }

        let Some(args) = call.args.plain() else {
            return Ok(match name {
                "concat" | "filter" | "sort" | "toSorted" => stripped,
                "max" | "min" => Ty::NUMBER,
                _ => Ty::OPEN,
            });
        };
        if let Some(builtin) = builtin {
            let result = if let (Builtin::Method(Method::StringSplit), [separator], [ty]) =
                (builtin, args, arg_tys.as_slice())
                && let TyKind::Regex(shape) = self.search_pattern(s.inst, separator, *ty)
            {
                MethodResult::Signature(shape.split().result_kinds())
            } else {
                builtin.result()
            };
            return Ok(self.method_result(
                s.inst,
                result,
                MethodResultArgs::call(obj_ty, args, &arg_tys, self.hir.sparse_arrays),
            ));
        }
        let is_known_js_cb_method = Method::accepts_callback(name)
            || matches!(name, "then" | "catch" | "finally" | "groupBy");
        let is_collection = matches!(
            self.types.kind(stripped),
            TyKind::Array(_) | TyKind::Cell { .. }
        ) || matches!(self.types.kind(stripped), TyKind::Generic { base, .. }
            if crate::source::CollectionKind::from_type_name(base.get(self.ctx)).is_some());
        let skip_host_cb = is_collection || (self.types.is_open(stripped) && is_known_js_cb_method);
        if hir_compat::has_source_offset(byte_offset) && !skip_host_cb {
            self.check_host_arguments(s, name, &arg_tys, byte_offset)?;
        }
        Ok(Ty::OPEN)
    }
}
