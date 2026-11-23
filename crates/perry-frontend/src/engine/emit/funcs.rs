//! Synthetic function-value records, dispatchers, wideners, comparers, and callback wrappers.
use super::{Emitter, EnvField, RewriteCtx};
use crate::engine::solve::FuncCoercion;
use crate::engine::types::{FuncIdentity, InstanceId, TemplateId, Ty, TyKind, func_tag};
use perry_hir::{
    ir::{Class, ClassField, ClassFieldOrigin, CompareOp, Expr, Function, Param, Stmt, SwitchCase},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result};

impl<'h> Emitter<'h, '_> {
    pub(super) fn is_boxed_local(&self, id: LocalId) -> bool {
        self.index
            .local_idx(id)
            .is_some_and(|li| self.solved.boxed_locals.contains(li.index()))
    }

    pub(super) fn local_ty_in_owner(&self, owner: InstanceId, id: LocalId) -> Option<Ty> {
        let mut cur = Some(owner);
        while let Some(c) = cur {
            if let Some(inst) = self.solved.instances.get(c)
                && let Some(view) = self.index.view(inst.template)
                && let Some(locs) = self.solved.param_locs.get(inst.param_locs.range())
                && let Some(&loc) = view
                    .params
                    .iter()
                    .zip(locs)
                    .find_map(|(p, loc)| (p.id == id).then_some(loc))
                && let Some(&ty) = self.solved.locations.get(loc)
                && !matches!(ty, Ty::BOTTOM | Ty::OPEN)
            {
                return Some(ty);
            }
            if let Some(ty) = self.local_ty(c, id)
                && !matches!(ty, Ty::BOTTOM | Ty::OPEN)
            {
                return Some(ty);
            }
            cur = self.solved.instances.get(c).and_then(|i| i.capture_owner);
        }
        None
    }

    pub(super) fn env_layout(&self, ident: FuncIdentity) -> (Vec<EnvField>, Vec<LocalId>) {
        let caps = self.index.template_captures(ident.template);
        let mut fields = Vec::with_capacity(caps.len());
        let mut self_caps = Vec::new();
        for &cap_id in caps {
            let cap_ty = ident
                .capture_owner
                .and_then(|o| self.local_ty_in_owner(o, cap_id))
                .unwrap_or(Ty::OPEN);
            if self.is_zero_sized_func(cap_ty) {
                continue;
            }
            if matches!(self.types.kind(cap_ty), TyKind::Func(cident) if cident == ident) {
                self_caps.push(cap_id);
                continue;
            }
            let boxed = self.is_boxed_local(cap_id);
            let name = format!("c{}", fields.len());
            fields.push((cap_id, name, cap_ty, boxed));
        }
        (fields, self_caps)
    }

    pub(super) fn materialize(&self, ty: Ty) -> HirType {
        self.types.materialize_hir_with(
            self.ctx,
            ty,
            self.solved.locations.as_slice(),
            &self.solved.dict_reads,
            &self.index.capture_free_templates,
            &self.solved.escaping_env_templates,
            &|cid| self.class_inst_name(cid),
            &|ident| {
                let (fields, _) = self.env_layout(ident);
                if fields.is_empty() {
                    vec![("_u".to_owned(), Ty::NUMBER, false)]
                } else {
                    fields
                        .into_iter()
                        .map(|(_, name, fty, boxed)| (name, fty, boxed))
                        .collect()
                }
            },
        )
    }

    pub(super) fn is_zero_sized_func(&self, ty: Ty) -> bool {
        self.types
            .is_zero_sized_func(ty, &self.index.capture_free_templates)
    }

    pub(super) fn is_env_template(&self, tid: TemplateId) -> bool {
        self.solved.escaping_env_templates.contains(tid.index())
    }

    pub(super) fn intern_anon_shape(&mut self, field_names: &[&str]) -> String {
        let key: Vec<String> = field_names.iter().map(|&n| n.to_owned()).collect();
        if let Some(existing) = self.synth_shapes.get(&key) {
            return existing.clone();
        }
        let id = self.next_class_id;
        self.next_class_id = id.saturating_add(1);
        let name = format!("__AnonShape_e_{id}");
        let fields = key
            .iter()
            .map(|fname| ClassField {
                origin: ClassFieldOrigin::Declared,
                name: fname.clone(),
                key_expr: None,
                ty: HirType::Any,
                init: None,
                is_private: false,
                is_readonly: false,
                decorators: Vec::new(),
            })
            .collect();
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
            constructor: None,
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
        self.synth_shapes.insert(key, name.clone());
        name
    }

    pub(super) fn emit_anon_new(&mut self, field_names: &[&str], args: Vec<Expr>) -> Expr {
        let class_name = self.intern_anon_shape(field_names);
        Expr::New {
            class_name,
            args,
            type_args: Vec::new(),
            byte_offset: 0,
            byte_end: 0,
            cap_args_appended: 0,
        }
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

    pub(super) fn emit_tag_record(&mut self, ident: FuncIdentity) -> Expr {
        self.emit_anon_new(&["tag"], vec![Expr::String(func_tag(ident))])
    }

    pub(super) fn emit_env_wrapper(&mut self, ident: FuncIdentity, env_expr: Expr) -> Expr {
        let tag = func_tag(ident);
        self.emit_env_wrapper_tag(&tag, env_expr)
    }

    pub(super) fn emit_env_wrapper_tag(&mut self, tag: &str, env_expr: Expr) -> Expr {
        self.emit_anon_new(
            &["tag", "val"],
            vec![Expr::String(tag.to_owned()), env_expr],
        )
    }

    pub(super) fn emit_box_record(&mut self, val: Expr) -> Expr {
        self.emit_anon_new(&["v"], vec![val])
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

    pub(super) fn coercion_of(&self, inst_id: InstanceId, expr: &Expr) -> Option<FuncCoercion> {
        let key = std::ptr::from_ref(expr) as usize;
        self.solved
            .inst_coercions
            .get(inst_id.index())?
            .iter()
            .find(|&&(k, _)| k == key)
            .map(|&(_, c)| c)
    }

    pub(super) fn arm_return_hir(&self, inst_id: InstanceId) -> HirType {
        let Some(inst) = self.solved.instances.get(inst_id) else {
            return HirType::Void;
        };
        let ret = self
            .solved
            .locations
            .get(inst.return_loc)
            .copied()
            .unwrap_or(Ty::VOID);
        if matches!(ret, Ty::BOTTOM | Ty::VOID | Ty::NEVER) || self.is_zero_sized_func(ret) {
            HirType::Void
        } else {
            self.materialize(ret)
        }
    }

    pub(super) fn synth_param(id: LocalId, name: String, ty: HirType) -> Param {
        Param {
            id,
            name,
            ty,
            default: None,
            is_rest: false,
            decorators: Vec::new(),
            arguments_object: None,
        }
    }

    pub(super) fn synth_call(
        fid: FuncId,
        args: Vec<Expr>,
        byte_offset: u32,
        byte_end: u32,
    ) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::FuncRef(fid)),
            args,
            type_args: Vec::new(),
            byte_offset,
            byte_end,
        }
    }

    pub(super) fn synth_function(
        id: FuncId,
        name: String,
        params: Vec<Param>,
        return_type: HirType,
        body: Vec<Stmt>,
        is_async: bool,
    ) -> Function {
        Function {
            id,
            name,
            type_params: Vec::new(),
            params,
            return_type,
            body,
            is_async,
            is_generator: false,
            is_exported: false,
            captures: Vec::new(),
            decorators: Vec::new(),
            is_strict: true,
            construct_only: false,
            was_unrolled: false,
            was_plain_async: false,
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
            params.push(Self::synth_param(
                pid,
                format!("__cb{i}"),
                self.materialize(ity),
            ));
            call_args.push(Expr::LocalGet(pid));
        }
        let call = Self::synth_call(target_fid, call_args, 0, 0);
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

    pub(super) fn intern_dispatcher(
        &mut self,
        callee_ty: Ty,
        arms: &[(FuncIdentity, InstanceId)],
        arg_tys: Ty,
        strip_arg_mask: u64,
        recv_ty: Option<Ty>,
    ) -> Result<(FuncId, bool)> {
        let needs_recv = recv_ty.is_some()
            && arms
                .iter()
                .any(|&(ident, _)| self.index.view(ident.template).is_some_and(|v| v.uses_this));
        let eff_recv = needs_recv.then_some(recv_ty).flatten();
        let key = (callee_ty, arms.to_vec(), arg_tys, strip_arg_mask, eff_recv);
        if let Some(&fid) = self.dispatchers.get(&key) {
            return Ok((fid, needs_recv));
        }
        let fid = self.fresh_func_id()?;
        let fn_pid = self.fresh_synthetic_local()?;
        let mut params = vec![Self::synth_param(
            fn_pid,
            "__fn".into(),
            self.materialize(callee_ty),
        )];
        let self_pid = if let Some(rty) = eff_recv {
            let sid = self.fresh_synthetic_local()?;
            params.push(Self::synth_param(
                sid,
                "__self".into(),
                self.materialize(rty),
            ));
            Some(sid)
        } else {
            None
        };
        let mut arg_pids = Vec::new();
        for (i, &aty) in self.types.tuple_slice(arg_tys).iter().enumerate() {
            if i < 64 && (strip_arg_mask & (1u64 << i)) != 0 {
                continue;
            }
            let pid = self.fresh_synthetic_local()?;
            params.push(Self::synth_param(
                pid,
                format!("__p{i}"),
                self.materialize(aty),
            ));
            arg_pids.push(pid);
        }
        let mut unique_rets: Vec<HirType> = Vec::new();
        let mut any_async = false;
        for &(ident, target_inst) in arms {
            if self.index.view(ident.template).is_some_and(|v| v.is_async) {
                any_async = true;
            }
            let r = self.arm_return_hir(target_inst);
            if !unique_rets.contains(&r) {
                unique_rets.push(r);
            }
        }
        let disp_ret = match unique_rets.as_slice() {
            [] => HirType::Void,
            [single] => single.clone(),
            _ => HirType::Union(unique_rets),
        };
        let mut cases = Vec::with_capacity(arms.len());
        for &(ident, target_inst) in arms {
            let target_fid = self
                .instance(target_inst, "missing dispatch arm instance")?
                .emitted_func_id;
            let view = self
                .index
                .view(ident.template)
                .ok_or_else(|| Diagnostic::internal("missing dispatch arm template"))?;
            let mut call_args = Vec::with_capacity(arg_pids.len() + 2);
            if self.is_env_template(ident.template) {
                call_args.push(Self::prop_get(Expr::LocalGet(fn_pid), "val"));
            }
            if view.uses_this
                && let Some(sid) = self_pid
            {
                call_args.push(Expr::LocalGet(sid));
            }
            let has_rest = view.params.iter().any(|p| p.is_rest);
            let max_args = if has_rest {
                arg_pids.len()
            } else {
                view.params
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i >= 64 || (strip_arg_mask & (1u64 << i)) == 0)
                    .count()
            };
            for &pid in arg_pids.iter().take(max_args) {
                call_args.push(Expr::LocalGet(pid));
            }
            let call = Self::synth_call(target_fid, call_args, 0, 0);
            let arm_ret = self.arm_return_hir(target_inst);
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
        let switch = Stmt::Switch {
            discriminant: Self::prop_get(Expr::LocalGet(fn_pid), "tag"),
            cases,
        };
        let func = Self::synth_function(
            fid,
            format!("__dispatch_{fid}"),
            params,
            disp_ret,
            vec![switch],
            any_async,
        );
        self.synth_functions.push(func);
        self.dispatchers.insert(key, fid);
        Ok((fid, needs_recv))
    }

    pub(super) fn intern_widener(&mut self, from: Ty, to: Ty) -> Result<FuncId> {
        if let Some(&fid) = self.wideners.get(&(from, to)) {
            return Ok(fid);
        }
        let fid = self.fresh_func_id()?;
        let pid = self.fresh_synthetic_local()?;
        let idents = self.types.func_members(from);
        let (has_null, has_undef) = self.nullish_flags(from);
        let mut stmts = Vec::new();
        if has_null {
            stmts.push(Self::nullish_guard(pid, Expr::Null, Expr::Null));
        }
        if has_undef {
            stmts.push(Self::nullish_guard(pid, Expr::Undefined, Expr::Undefined));
        }
        let mut cases = Vec::with_capacity(idents.len());
        for ident in idents {
            let val = if self.is_env_template(ident.template) {
                let env_val = Self::prop_get(Expr::LocalGet(pid), "val");
                self.emit_env_wrapper(ident, env_val)
            } else {
                self.emit_tag_record(ident)
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
        let func = Self::synth_function(
            fid,
            format!("__widen_{fid}"),
            vec![Self::synth_param(
                pid,
                "__fn".into(),
                self.materialize(from),
            )],
            self.materialize(to),
            stmts,
            false,
        );
        self.synth_functions.push(func);
        self.wideners.insert((from, to), fid);
        Ok(fid)
    }

    pub(super) fn intern_comparer(&mut self, left_ty: Ty, right_ty: Ty) -> Result<FuncId> {
        if let Some(&fid) = self.comparers.get(&(left_ty, right_ty)) {
            return Ok(fid);
        }
        let fid = self.fresh_func_id()?;
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
        let l_members = self.types.func_members(left_ty);
        let r_members = self.types.func_members(right_ty);
        for ident in l_members {
            if !r_members.contains(&ident) {
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
        let func = Self::synth_function(
            fid,
            format!("__cmp_{fid}"),
            vec![
                Self::synth_param(a_pid, "__a".into(), self.materialize(left_ty)),
                Self::synth_param(b_pid, "__b".into(), self.materialize(right_ty)),
            ],
            HirType::Boolean,
            stmts,
            false,
        );
        self.synth_functions.push(func);
        self.comparers.insert((left_ty, right_ty), fid);
        Ok(fid)
    }

    pub(super) fn intern_caster(&mut self, ty: Ty) -> Result<FuncId> {
        if let Some(&fid) = self.casters.get(&ty) {
            return Ok(fid);
        }
        let fid = self.fresh_func_id()?;
        let pid = self.fresh_synthetic_local()?;
        let hty = self.materialize(ty);
        let func = Self::synth_function(
            fid,
            format!("__cast_{fid}"),
            vec![Self::synth_param(pid, "__v".into(), hty.clone())],
            hty,
            vec![Stmt::Return(Some(Expr::LocalGet(pid)))],
            false,
        );
        self.synth_functions.push(func);
        self.casters.insert(ty, fid);
        Ok(fid)
    }

    fn nullish_flags(&self, ty: Ty) -> (bool, bool) {
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
