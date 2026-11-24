//! Synthetic function-value records, dispatchers, wideners, comparers, and callback wrappers.
use super::{Emitter, EnvField, RewriteCtx, synth_call, synth_function, synth_param};
use crate::engine::bitset::BitSet;
use crate::engine::boundary::TemplateKind;
use crate::engine::solve::HelperSpec;
use crate::engine::types::{
    CaptureOwner, DispatchArmSpan, FuncIdentity, HelperId, InstanceId, TemplateId, Ty, TyKind,
    box_hir, env_class_name, func_tag,
};
use crate::hir_compat;
use perry_hir::{
    ir::{Class, ClassField, ClassFieldOrigin, CompareOp, Expr, Function, Stmt, SwitchCase},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};

impl<'h, 'a> Emitter<'h, 'a> {
    pub(super) fn is_boxed_local(&self, id: LocalId) -> bool {
        self.index
            .local_idx(id)
            .is_some_and(|li| self.solved.boxed_locals.contains(li.index()))
    }

    pub(super) fn is_forward_boxed(&self, id: LocalId) -> bool {
        self.index.local_idx(id).is_some_and(|li| {
            self.solved.boxed_locals.contains(li.index())
                && self.index.local_forward_captured.contains(li.index())
        })
    }

    pub(super) fn is_undef_boxed(&self, id: LocalId) -> bool {
        self.index.local_idx(id).is_some_and(|li| {
            self.solved.boxed_locals.contains(li.index())
                && (self.index.local_forward_captured.contains(li.index())
                    || self.index.local_deferred.contains(li.index()))
        })
    }

    pub(super) fn admitting_undefined_hir(ty: HirType) -> HirType {
        match ty {
            HirType::Void => HirType::Void,
            HirType::Union(mut members) => {
                if !members.contains(&HirType::Void) {
                    members.push(HirType::Void);
                }
                HirType::Union(members)
            }
            other => HirType::Union(vec![other, HirType::Void]),
        }
    }

    pub(super) fn local_ty_in_owner(&self, owner: CaptureOwner, id: LocalId) -> Option<Ty> {
        let mut cur = owner;
        loop {
            match cur {
                CaptureOwner::Instance(c) => {
                    let inst = &self.solved.instances[c];
                    let locs = &self.solved.param_locs[inst.param_locs];
                    if let Some(&loc) = self
                        .index
                        .view(inst.template)
                        .params
                        .iter()
                        .zip(locs)
                        .find_map(|(p, loc)| (p.id == id).then_some(loc))
                        && let ty = self.solved.locations[loc]
                        && !matches!(ty, Ty::BOTTOM | Ty::OPEN)
                    {
                        return Some(ty);
                    }
                    if let Some(ty) = self.local_ty(c, id)
                        && !matches!(ty, Ty::BOTTOM | Ty::OPEN)
                    {
                        return Some(ty);
                    }
                    cur = inst.capture_owner;
                }
                CaptureOwner::Template(_) => {
                    let lidx = self.index.local_idx(id)?;
                    return self.solved.collapsed_local_tys[lidx]
                        .filter(|&ty| !matches!(ty, Ty::BOTTOM | Ty::OPEN));
                }
                CaptureOwner::None => return None,
            }
        }
    }

    pub(super) fn env_layout(&self, ident: FuncIdentity) -> Result<Vec<EnvField>> {
        let caps = self.index.template_captures(ident.template);
        let mut fields = Vec::with_capacity(caps.len());
        for &cap_id in caps {
            let cap_ty = match self.local_ty_in_owner(ident.capture_owner, cap_id) {
                Some(ty) => ty,
                None => {
                    let err = self.annotate_local(
                        diag!(
                            codes::MISSING_TYPE_ANNOTATION,
                            "the value type cannot be inferred"
                        ),
                        cap_id,
                    );
                    return Err(match ident.capture_owner.instance() {
                        Some(owner) => self.attach_inst_chain(err, owner),
                        None => err,
                    });
                }
            };
            if self.is_zero_sized_func(cap_ty) {
                continue;
            }
            let boxed = self.is_boxed_local(cap_id);
            let name = format!("c{}", fields.len());
            fields.push((cap_id, name, cap_ty, boxed));
        }
        Ok(fields)
    }

    pub(super) fn ensure_env_class(&mut self, ident: FuncIdentity) -> Result<String> {
        let name = env_class_name(ident);
        if !self.emitted_env_classes.insert(ident) {
            return Ok(name);
        }
        if matches!(
            self.index.view(ident.template).kind,
            TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
        ) {
            return self.emit_promise_env_class(ident, name);
        }
        let layout = self.env_layout(ident)?;
        let mut members = Vec::with_capacity(layout.len().max(1));
        for (cap_id, field, ty, boxed) in layout {
            let ty = self.materialize(ty)?;
            let ty = if boxed {
                box_hir(if self.is_undef_boxed(cap_id) {
                    Self::admitting_undefined_hir(ty)
                } else {
                    ty
                })
            } else {
                ty
            };
            members.push((field, ty));
        }
        if members.is_empty() {
            members.push(("_u".to_owned(), HirType::Number));
        }
        self.class_with_fields(name, members)
    }

    pub(super) fn class_with_fields(
        &mut self,
        name: String,
        members: impl IntoIterator<Item = (String, HirType)>,
    ) -> Result<String> {
        let mut fields = Vec::new();
        let mut params = Vec::new();
        let mut body = Vec::new();
        for (field, ty) in members {
            let pid = self.fresh_synthetic_local()?;
            params.push(synth_param(pid, field.clone(), ty.clone()));
            body.push(Stmt::Expr(Expr::PropertySet {
                object: Box::new(Expr::This),
                property: field.clone(),
                value: Box::new(Expr::LocalGet(pid)),
                byte_offset: 0,
                byte_end: 0,
            }));
            fields.push(ClassField {
                origin: ClassFieldOrigin::Declared,
                name: field,
                key_expr: None,
                ty,
                init: None,
                is_private: false,
                is_readonly: false,
                decorators: Vec::new(),
            });
        }
        let ctor_fid = self.fresh_func_id()?;
        let ctor = synth_function(
            ctor_fid,
            "constructor".to_owned(),
            params,
            HirType::Void,
            body,
            false,
        );
        let id = self.next_class_id;
        self.next_class_id = id
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("class identifier limit"))?;
        self.synth_classes.push(Class {
            id,
            name: name.clone(),
            type_params: Vec::new(),
            extends: None,
            extends_name: None,
            native_extends: None,
            extends_expr: None,
            heritage_lexically_shadowed: false,
            fields,
            constructor: Some(ctor),
            methods: Vec::new(),
            getters: Vec::new(),
            setters: Vec::new(),
            static_accessor_names: Vec::new(),
            static_accessor_fn_ids: Vec::new(),
            static_fields: Vec::new(),
            static_methods: Vec::new(),
            computed_members: Vec::new(),
            decorators: Vec::new(),
            is_exported: false,
            aliases: Vec::new(),
            is_nested: false,
            alloc_width_hint: 0,
            specialized_from: None,
            decl_stmt: None,
        });
        Ok(name)
    }

    fn collect_env_idents_in(
        &self,
        ty: Ty,
        seen: &mut BitSet,
        out: &mut Vec<FuncIdentity>,
        needs_timer: &mut bool,
    ) {
        if !seen.insert(ty.index()) {
            return;
        }
        match self.types.kind(ty) {
            TyKind::Leaf(id) => {
                *needs_timer |= crate::hir_compat::is_timer_handle(id.hir(self.ctx));
            }
            TyKind::Func(ident) => {
                if self.is_env_template(ident.template)
                    && !self.emitted_env_classes.contains(&ident)
                {
                    out.push(ident);
                }
            }
            TyKind::Array(i) | TyKind::Promise(i) => {
                self.collect_env_idents_in(i, seen, out, needs_timer)
            }
            TyKind::Cell { loc, .. } => {
                self.collect_env_idents_in(self.solved.locations[loc], seen, out, needs_timer);
            }
            TyKind::Tuple(s) | TyKind::Union(s) | TyKind::Generic { args: s, .. } => {
                for &m in self.types.ty_slice(s) {
                    self.collect_env_idents_in(m, seen, out, needs_timer);
                }
            }
            TyKind::Object { props, index } => {
                if let Some(i) = index {
                    self.collect_env_idents_in(i, seen, out, needs_timer);
                }
                for p in self.types.object_slice(props) {
                    self.collect_env_idents_in(p.ty, seen, out, needs_timer);
                }
            }
            _ => {}
        }
    }

    pub(super) fn materialize(&mut self, ty: Ty) -> Result<HirType> {
        let mut idents = Vec::new();
        let mut needs_timer = false;
        self.collect_env_idents_in(ty, &mut BitSet::default(), &mut idents, &mut needs_timer);
        if needs_timer {
            let _ = self.ensure_timer_class()?;
        }
        for ident in idents {
            self.ensure_env_class(ident)?;
        }
        Ok(self.types.materialize_hir_with(
            self.ctx,
            ty,
            self.solved.locations.as_slice(),
            &self.solved.dict_reads,
            &self.index.capture_free_templates,
            &self.solved.escaping_env_templates,
            &|cid| self.solved.class_insts[cid].emitted_name.as_str(),
        ))
    }

    pub(super) fn coerce_capture_expr(
        &mut self,
        arg: Expr,
        src_ty: Ty,
        dst_ty: Ty,
    ) -> Result<Expr> {
        if !self.types.is_tagged_func_union(
            dst_ty,
            &self.index.capture_free_templates,
            &self.solved.escaping_env_templates,
        ) {
            return Ok(arg);
        }
        if self.is_zero_sized_func(src_ty)
            && let TyKind::Func(ident) = self.types.kind(src_ty)
        {
            return Ok(Self::emit_tag_record(ident));
        }
        if self
            .types
            .is_env_func(src_ty, &self.solved.escaping_env_templates)
            && let TyKind::Func(ident) = self.types.kind(src_ty)
        {
            return Ok(Self::emit_env_wrapper(ident, Self::prop_get(arg, "val")));
        }
        if self.types.is_tagged_func_union(
            src_ty,
            &self.index.capture_free_templates,
            &self.solved.escaping_env_templates,
        ) && src_ty != dst_ty
        {
            let target_spec = HelperSpec::Widener {
                from: src_ty,
                to: dst_ty,
            };
            let helper = self
                .solved
                .helpers
                .iter_enumerated()
                .find_map(|(id, &spec)| (spec == target_spec).then_some(id))
                .ok_or_else(|| {
                    Diagnostic::internal("missing interned widener for collapsed capture")
                })?;
            let fid = self.emit_helper(helper)?;
            return Ok(synth_call(fid, vec![arg], 0, 0));
        }
        Ok(arg)
    }

    pub(super) fn is_zero_sized_func(&self, ty: Ty) -> bool {
        self.types
            .is_zero_sized_func(ty, &self.index.capture_free_templates)
    }

    pub(super) fn is_env_template(&self, tid: TemplateId) -> bool {
        self.solved.escaping_env_templates.contains(tid.index())
    }

    pub(super) fn fresh_synthetic_local(&mut self) -> Result<LocalId> {
        let next = self.next_local;
        self.next_local = next
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("local identifier limit"))?;
        Ok(next)
    }

    pub(super) fn fresh_func_id(&mut self) -> Result<FuncId> {
        let next = self.next_func_id;
        self.next_func_id = next
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("function identifier limit"))?;
        Ok(next)
    }

    pub(super) fn emit_tag_record(ident: FuncIdentity) -> Expr {
        Expr::Object(vec![("tag".into(), Expr::String(func_tag(ident)))])
    }

    pub(super) fn emit_env_wrapper(ident: FuncIdentity, env_expr: Expr) -> Expr {
        Expr::Object(vec![
            ("tag".into(), Expr::String(func_tag(ident))),
            ("val".into(), env_expr),
        ])
    }

    pub(super) fn emit_box_record(val: Expr) -> Expr {
        Expr::Object(vec![("v".into(), val)])
    }

    pub(super) fn prop_get(object: Expr, property: &str) -> Expr {
        Expr::PropertyGet {
            object: Box::new(object),
            property: property.to_owned(),
            byte_offset: 0,
            byte_end: 0,
        }
    }

    pub(super) fn emit_capture_ref(&self, rctx: &RewriteCtx<'_>, cap_id: LocalId) -> Expr {
        if let Some(env_pid) = rctx.env_local
            && let Some((_, field, _)) = rctx.env_captures.iter().find(|(lid, _, _)| *lid == cap_id)
        {
            return Self::prop_get(Expr::LocalGet(env_pid), field);
        }
        if let Some(&(_, box_id)) = rctx.boxed_param_map.iter().find(|(lid, _)| *lid == cap_id) {
            return Expr::LocalGet(box_id);
        }
        Expr::LocalGet(cap_id)
    }

    pub(super) fn arm_return_hir(&mut self, inst_id: InstanceId) -> Result<HirType> {
        let inst = &self.solved.instances[inst_id];
        let ret = self.solved.locations[inst.return_loc];
        if matches!(ret, Ty::BOTTOM | Ty::VOID | Ty::NEVER) || self.is_zero_sized_func(ret) {
            Ok(HirType::Void)
        } else {
            self.materialize(ret)
        }
    }

    pub(super) fn emit_callback_wrapper(
        &mut self,
        target_fid: FuncId,
        first_arg: Expr,
        inputs: &[Ty],
        ret_ty: HirType,
    ) -> Result<Expr> {
        let mut captures = Vec::new();
        let mut defined = std::collections::HashSet::new();
        perry_hir::analysis::collect_local_refs_expr(&first_arg, &mut captures, &mut defined);
        let mut params = Vec::with_capacity(inputs.len());
        let mut call_args = Vec::with_capacity(inputs.len() + 1);
        call_args.push(first_arg);
        for (i, &ity) in inputs.iter().enumerate() {
            let pid = self.fresh_synthetic_local()?;
            params.push(synth_param(pid, format!("__cb{i}"), self.materialize(ity)?));
            if !self.erases_argument(ity) {
                call_args.push(Expr::LocalGet(pid));
            }
        }
        let call = synth_call(target_fid, call_args, 0, 0);
        let body = if ret_ty == HirType::Void {
            vec![Stmt::Expr(call)]
        } else {
            vec![Stmt::Return(Some(call))]
        };
        let closure_fid = self.fresh_func_id()?;
        Ok(Expr::Closure {
            func_id: closure_fid,
            params,
            return_type: ret_ty,
            body,
            captures,
            mutable_captures: Vec::new(),
            captures_this: false,
            captures_new_target: false,
            enclosing_class: None,
            is_async: false,
            is_generator: false,
            is_strict: true,
            construct_only: false,
            is_arrow: true,
        })
    }

    pub(super) fn emit_helper_fn(&mut self, id: HelperId) -> Result<&Function> {
        let slot = if let Some(slot) = self.helper_slots[id] {
            slot
        } else {
            let spec = self.solved.helpers[id];
            let byte_offset = self.solved.helper_byte_offsets[id];
            let fid = self.fresh_func_id()?;
            let func = match spec {
                HelperSpec::ThrowTypeError => self.build_throw_type_error(fid),
                HelperSpec::TypeOfFn(op_ty) => self.build_typeof_fn(fid, op_ty)?,
                HelperSpec::Dispatcher {
                    callee_ty,
                    arms,
                    arg_tys,
                    recv_ty,
                } => self.build_dispatcher(fid, callee_ty, arms, arg_tys, recv_ty, byte_offset)?,
                HelperSpec::Widener { from, to } => self.build_widener(fid, from, to)?,
                HelperSpec::Comparer { left_ty, right_ty } => {
                    self.build_comparer(fid, left_ty, right_ty)?
                }
                HelperSpec::PromiseReaction {
                    kind,
                    promise_ty,
                    inner_ty,
                    on_fulfilled,
                    on_rejected,
                    ret_ty,
                } => self.build_promise_reaction(
                    fid,
                    kind,
                    promise_ty,
                    inner_ty,
                    on_fulfilled,
                    on_rejected,
                    ret_ty,
                )?,
                HelperSpec::TimerRunner {
                    repeating,
                    is_immediate,
                    cb_call,
                    cb_ty,
                    extra_arg_tys,
                } => self.build_timer_runner(
                    fid,
                    repeating,
                    is_immediate,
                    cb_call,
                    cb_ty,
                    extra_arg_tys,
                )?,
            };
            let slot = self.synth_functions.len();
            self.synth_functions.push(func);
            self.helper_slots[id] = Some(slot);
            slot
        };
        Ok(&self.synth_functions[slot])
    }

    pub(super) fn emit_helper(&mut self, id: HelperId) -> Result<FuncId> {
        Ok(self.emit_helper_fn(id)?.id)
    }

    fn build_throw_type_error(&self, fid: FuncId) -> Function {
        let body = vec![Stmt::Throw(Expr::TypeErrorNew(Box::new(Expr::String(
            "callee is not a function".into(),
        ))))];
        synth_function(
            fid,
            format!("__not_fn_{fid}"),
            Vec::new(),
            HirType::Never,
            body,
            false,
        )
    }

    fn build_typeof_fn(&mut self, fid: FuncId, op_ty: Ty) -> Result<Function> {
        let pid = self.fresh_synthetic_local()?;
        let (has_null, has_undef) = self.nullish_flags(op_ty);
        let mut stmts = Vec::new();
        if has_null {
            stmts.push(Self::nullish_guard(
                pid,
                Expr::Null,
                Expr::String("object".into()),
            ));
        }
        if has_undef {
            stmts.push(Self::nullish_guard(
                pid,
                Expr::Undefined,
                Expr::String("undefined".into()),
            ));
        }
        stmts.push(Stmt::Return(Some(Expr::String("function".into()))));
        Ok(synth_function(
            fid,
            format!("__typeof_fn_{fid}"),
            vec![synth_param(pid, "__v".into(), self.materialize(op_ty)?)],
            HirType::String,
            stmts,
            false,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_dispatcher(
        &mut self,
        fid: FuncId,
        callee_ty: Ty,
        arms_span: DispatchArmSpan,
        arg_tys: Ty,
        eff_recv: Option<Ty>,
        call_byte_offset: u32,
    ) -> Result<Function> {
        let arms = &self.solved.dispatch_arms[arms_span];
        let fn_pid = self.fresh_synthetic_local()?;
        let mut params = vec![synth_param(
            fn_pid,
            "__fn".into(),
            self.materialize(callee_ty)?,
        )];
        let self_pid = if let Some(rty) = eff_recv {
            let sid = self.fresh_synthetic_local()?;
            params.push(synth_param(sid, "__self".into(), self.materialize(rty)?));
            Some(sid)
        } else {
            None
        };
        let mut arg_pids = Vec::new();
        for (i, &aty) in self.types.tuple_slice(arg_tys).iter().enumerate() {
            if self.erases_argument(aty) {
                continue;
            }
            let pid = self.fresh_synthetic_local()?;
            params.push(synth_param(pid, format!("__p{i}"), self.materialize(aty)?));
            arg_pids.push(pid);
        }
        let mut unique_rets: Vec<HirType> = Vec::new();
        let mut any_async = false;
        for &(ident, target_inst) in arms {
            if self.index.view(ident.template).is_async {
                any_async = true;
            }
            let r = self.arm_return_hir(target_inst)?;
            if !unique_rets.contains(&r) {
                unique_rets.push(r);
            }
        }
        let disp_ret = match unique_rets.as_slice() {
            [] => HirType::Void,
            [single] => single.clone(),
            _ => HirType::Union(unique_rets),
        };
        let (has_null, has_undef) = self.nullish_flags(callee_ty);
        let mut stmts = Vec::new();
        if has_null {
            stmts.push(Stmt::If {
                condition: Self::eq_expr(Expr::LocalGet(fn_pid), Expr::Null),
                then_branch: vec![Stmt::Throw(Expr::TypeErrorNew(Box::new(Expr::String(
                    "callee is not a function".into(),
                ))))],
                else_branch: None,
            });
        }
        if has_undef {
            stmts.push(Stmt::If {
                condition: Self::eq_expr(Expr::LocalGet(fn_pid), Expr::Undefined),
                then_branch: vec![Stmt::Throw(Expr::TypeErrorNew(Box::new(Expr::String(
                    "callee is not a function".into(),
                ))))],
                else_branch: None,
            });
        }
        let mut cases = Vec::with_capacity(arms.len());
        for &(ident, target_inst) in arms {
            let target = &self.solved.instances[target_inst];
            let target_fid = target.emitted_func_id;
            let view = self.index.view(ident.template);
            let mut call_args = Vec::with_capacity(arg_pids.len() + 2);
            if self.is_env_template(ident.template) {
                call_args.push(Self::prop_get(Expr::LocalGet(fn_pid), "val"));
            }
            if view.uses_this
                && let Some(sid) = self_pid
            {
                call_args.push(Expr::LocalGet(sid));
            }
            let is_promise_handle = matches!(
                view.kind,
                TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
            );
            let has_rest = view.params.iter().any(|p| p.is_rest);
            let max_args = if is_promise_handle {
                self.types
                    .tuple_slice(target.args)
                    .len()
                    .min(arg_pids.len())
            } else if has_rest {
                arg_pids.len()
            } else {
                self.types
                    .tuple_slice(arg_tys)
                    .iter()
                    .take(view.params.len())
                    .filter(|&&ty| !self.erases_argument(ty))
                    .count()
            };
            for &pid in arg_pids.iter().take(max_args) {
                call_args.push(Expr::LocalGet(pid));
            }
            let call = synth_call(target_fid, call_args, 0, 0);
            let arm_ret = self.arm_return_hir(target_inst)?;
            let body = if disp_ret == HirType::Void {
                vec![Stmt::Expr(call), Stmt::Return(None)]
            } else if arm_ret == HirType::Void {
                vec![Stmt::Expr(call), Stmt::Return(Some(Expr::Undefined))]
            } else {
                vec![Stmt::Return(Some(call))]
            };
            cases.push(SwitchCase {
                test: Some(Expr::String(func_tag(ident))),
                body,
            });
        }
        if arms.len() == 1
            && let Some(case) = cases.pop()
        {
            stmts.extend(case.body);
        } else {
            stmts.push(Stmt::Switch {
                discriminant: Self::prop_get(Expr::LocalGet(fn_pid), "tag"),
                cases,
            });
        }
        let disp_name = format!("__dispatch_{fid}");
        if hir_compat::has_source_offset(call_byte_offset) {
            self.instantiations.push((
                disp_name.clone(),
                "dispatch".to_owned(),
                call_byte_offset,
                None,
            ));
        }
        Ok(synth_function(
            fid, disp_name, params, disp_ret, stmts, any_async,
        ))
    }

    fn build_widener(&mut self, fid: FuncId, from: Ty, to: Ty) -> Result<Function> {
        let pid = self.fresh_synthetic_local()?;
        let idents = self.types.func_members(&from);
        let (has_null, has_undef) = self.nullish_flags(from);
        let mut stmts = Vec::new();
        if has_null {
            stmts.push(Self::nullish_guard(pid, Expr::Null, Expr::Null));
        }
        if has_undef {
            stmts.push(Self::nullish_guard(pid, Expr::Undefined, Expr::Undefined));
        }
        let mut cases = Vec::new();
        for ident in idents {
            let val = if self.is_env_template(ident.template) {
                let env_val = Self::prop_get(Expr::LocalGet(pid), "val");
                Self::emit_env_wrapper(ident, env_val)
            } else {
                Self::emit_tag_record(ident)
            };
            cases.push(SwitchCase {
                test: Some(Expr::String(func_tag(ident))),
                body: vec![Stmt::Return(Some(val))],
            });
        }
        stmts.push(Stmt::Switch {
            discriminant: Self::prop_get(Expr::LocalGet(pid), "tag"),
            cases,
        });
        Ok(synth_function(
            fid,
            format!("__widen_{fid}"),
            vec![synth_param(pid, "__fn".into(), self.materialize(from)?)],
            self.materialize(to)?,
            stmts,
            false,
        ))
    }

    fn build_comparer(&mut self, fid: FuncId, left_ty: Ty, right_ty: Ty) -> Result<Function> {
        let a_pid = self.fresh_synthetic_local()?;
        let b_pid = self.fresh_synthetic_local()?;
        let (a_null, a_undef) = self.nullish_flags(left_ty);
        let (b_null, b_undef) = self.nullish_flags(right_ty);
        let mut stmts = Vec::new();
        if a_null {
            let ret = if b_null {
                Self::eq_expr(Expr::LocalGet(b_pid), Expr::Null)
            } else {
                Expr::Bool(false)
            };
            stmts.push(Self::nullish_guard(a_pid, Expr::Null, ret));
        }
        if a_undef {
            let ret = if b_undef {
                Self::eq_expr(Expr::LocalGet(b_pid), Expr::Undefined)
            } else {
                Expr::Bool(false)
            };
            stmts.push(Self::nullish_guard(a_pid, Expr::Undefined, ret));
        }
        if b_null {
            stmts.push(Self::nullish_guard(b_pid, Expr::Null, Expr::Bool(false)));
        }
        if b_undef {
            stmts.push(Self::nullish_guard(
                b_pid,
                Expr::Undefined,
                Expr::Bool(false),
            ));
        }
        let l_members = self.types.func_members(&left_ty);
        let r_members = self.types.func_members(&right_ty);
        for ident in l_members {
            if !r_members.clone().any(|member| member == ident) {
                continue;
            }
            let tag = func_tag(ident);
            let b_tag_eq = Self::eq_expr(
                Self::prop_get(Expr::LocalGet(b_pid), "tag"),
                Expr::String(tag.clone()),
            );
            let inner = if self.is_env_template(ident.template) {
                vec![
                    Stmt::If {
                        condition: b_tag_eq,
                        then_branch: vec![Stmt::Return(Some(Self::eq_expr(
                            Self::prop_get(Expr::LocalGet(a_pid), "val"),
                            Self::prop_get(Expr::LocalGet(b_pid), "val"),
                        )))],
                        else_branch: None,
                    },
                    Stmt::Return(Some(Expr::Bool(false))),
                ]
            } else {
                vec![Stmt::Return(Some(b_tag_eq))]
            };
            stmts.push(Stmt::If {
                condition: Self::eq_expr(
                    Self::prop_get(Expr::LocalGet(a_pid), "tag"),
                    Expr::String(tag),
                ),
                then_branch: inner,
                else_branch: None,
            });
        }
        stmts.push(Stmt::Return(Some(Expr::Bool(false))));
        Ok(synth_function(
            fid,
            format!("__cmp_{fid}"),
            vec![
                synth_param(a_pid, "__a".into(), self.materialize(left_ty)?),
                synth_param(b_pid, "__b".into(), self.materialize(right_ty)?),
            ],
            HirType::Boolean,
            stmts,
            false,
        ))
    }

    pub(super) fn nullish_flags(&self, ty: Ty) -> (bool, bool) {
        match self.types.kind(ty) {
            TyKind::Union(span) => {
                let s = self.types.ty_slice(span);
                (s.contains(&Ty::NULL), s.contains(&Ty::VOID))
            }
            _ => (false, false),
        }
    }

    fn eq_expr(left: Expr, right: Expr) -> Expr {
        Expr::Compare {
            op: CompareOp::Eq,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn nullish_guard(pid: LocalId, sentinel: Expr, ret: Expr) -> Stmt {
        Stmt::If {
            condition: Self::eq_expr(Expr::LocalGet(pid), sentinel),
            then_branch: vec![Stmt::Return(Some(ret))],
            else_branch: None,
        }
    }
}
