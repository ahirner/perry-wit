//! Closure environments, callback bindings, and finite function dispatch.
use super::{Emitter, EnvField, synth_call};
use crate::engine::Storage;
use crate::engine::bitset::BitSet;
use crate::engine::boundary::{LocalFlags, TemplateKind};
use crate::engine::solve::{CallableOp, HelperSpec};
use crate::engine::types::{
    CaptureOwner, DispatchArmSpan, FuncIdentity, HelperId, Idx, TemplateId, Ty, TyKind,
    env_class_name,
};
use crate::hir_compat;
use perry_hir::{
    ir::Expr,
    types::{FuncId, LocalId},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};

impl<'h, 'a> Emitter<'h, 'a> {
    pub(super) fn is_boxed_local(&self, id: LocalId) -> bool {
        self.index
            .local_idx(id)
            .is_some_and(|li| self.solved.boxed_locals.contains(li.index()))
    }

    pub(super) fn cell_init(&self, id: LocalId) -> crate::engine::CellInit {
        use crate::engine::CellInit;
        let flags = self
            .index
            .local_idx(id)
            .map(|local| self.index.locals[local].flags);
        if flags.is_some_and(|flags| flags.contains(LocalFlags::FORWARD_CAPTURED)) {
            CellInit::Hoisted
        } else if flags.is_some_and(|flags| flags.contains(LocalFlags::DEFERRED)) {
            CellInit::Deferred
        } else {
            CellInit::AtDeclaration
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
                    return self.solved.collapsed_local_locs[lidx]
                        .map(|loc| self.solved.locations[loc])
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

    pub(super) fn environment(&mut self, ident: FuncIdentity) -> Result<FuncId> {
        if let Some(&id) = self.environments.get(&ident) {
            return Ok(id);
        }
        let id = self.fresh_func_id()?;
        self.environments.insert(ident, id);
        let fields = match self.index.view(ident.template).kind {
            TemplateKind::PromiseResolve(site) | TemplateKind::PromiseReject(site) => {
                let loc =
                    super::SiteEntry::promise(&self.solved.inst_sites, ident.capture_owner, site)?;
                let elem = self.solved.locations[loc];
                self.prepare_layouts(elem)?;
                vec![("c0".into(), Storage::Promise(elem))]
            }
            _ => self.capture_fields(ident)?,
        };
        self.plans.operations.insert(
            id,
            crate::engine::NativeOperation::Allocate(crate::engine::RecordLayout {
                name: env_class_name(ident),
                function: Some(ident),
                fields,
            }),
        );
        Ok(id)
    }

    fn capture_fields(&mut self, ident: FuncIdentity) -> Result<Vec<(String, Storage)>> {
        let layout = self.env_layout(ident)?;
        let mut fields = Vec::with_capacity(layout.len().max(1));
        for (id, name, ty, boxed) in layout {
            self.prepare_layouts(ty)?;
            let storage = if boxed {
                Storage::Boxed {
                    ty,
                    init: self.cell_init(id),
                }
            } else {
                Storage::Value(ty)
            };
            fields.push((name, storage));
        }
        if fields.is_empty() {
            fields.push(("_u".into(), Storage::Value(Ty::NUMBER)));
        }
        Ok(fields)
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
            TyKind::Named(id) => {
                *needs_timer |= id.get(self.ctx) == crate::hir_compat::TIMER_CLASS;
            }
            TyKind::Func(ident) => {
                if self.is_env_template(ident.template) && !self.environments.contains_key(&ident) {
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

    pub(super) fn prepare_layouts(&mut self, ty: Ty) -> Result<()> {
        let mut idents = Vec::new();
        let mut needs_timer = false;
        self.collect_env_idents_in(ty, &mut BitSet::default(), &mut idents, &mut needs_timer);
        if needs_timer {
            self.timer_record()?;
        }
        for ident in idents {
            self.environment(ident)?;
        }
        Ok(())
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
            return self.expression_op(
                crate::engine::ExpressionOp::Function {
                    identity: ident,
                    target: dst_ty,
                },
                vec![],
            );
        }
        if self
            .types
            .is_env_func(src_ty, &self.solved.escaping_env_templates)
            && let TyKind::Func(ident) = self.types.kind(src_ty)
        {
            return self.expression_op(
                crate::engine::ExpressionOp::Function {
                    identity: ident,
                    target: dst_ty,
                },
                vec![arg],
            );
        }
        if self.types.is_tagged_func_union(
            src_ty,
            &self.index.capture_free_templates,
            &self.solved.escaping_env_templates,
        ) && src_ty != dst_ty
        {
            let target_spec = HelperSpec::Callable(CallableOp::Widener {
                from: src_ty,
                to: dst_ty,
            });
            let helper = self
                .solved
                .helpers
                .iter_enumerated()
                .find_map(|(id, entry)| (entry.spec == target_spec).then_some(id))
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

    pub(super) fn prop_get(object: Expr, property: &str) -> Expr {
        Expr::PropertyGet {
            object: Box::new(object),
            property: property.to_owned(),
            byte_offset: 0,
            byte_end: 0,
        }
    }

    pub(super) fn bind_callback(
        &mut self,
        target: FuncId,
        operand: crate::engine::CallbackOperand,
        expression: Expr,
        inputs: &[Ty],
    ) -> Result<Expr> {
        let mut params = Vec::with_capacity(inputs.len());
        for &ty in inputs {
            self.prepare_layouts(ty)?;
            params.push((!self.erases_argument(ty)).then_some(ty));
        }
        let id = self.fresh_func_id()?;
        self.plans.operations.insert(
            id,
            crate::engine::NativeOperation::Callback(crate::engine::CallbackBinding {
                call: crate::engine::NativeCallback::Call {
                    target,
                    operand,
                    receiver: false,
                },
                params,
            }),
        );
        Ok(synth_call(id, vec![expression], 0, 0))
    }

    pub(super) fn emit_helper(&mut self, id: HelperId) -> Result<FuncId> {
        if let Some(fid) = self.helper_slots[id] {
            return Ok(fid);
        }
        let entry = self.solved.helpers[id];
        let (spec, byte_offset) = (entry.spec, entry.byte_offset);
        let fid = self.fresh_func_id()?;
        self.helper_slots[id] = Some(fid);
        match spec {
            HelperSpec::Callable(operation) => {
                self.plans
                    .operations
                    .insert(fid, crate::engine::NativeOperation::Callable(operation));
            }
            HelperSpec::Dispatcher {
                result,
                callee_ty,
                arms,
                arg_tys,
                recv_ty,
            } => {
                let dispatcher =
                    self.build_dispatcher(fid, callee_ty, arms, arg_tys, recv_ty, result)?;
                self.plans.functions.insert(fid, dispatcher);
                if hir_compat::has_source_offset(byte_offset) {
                    self.instantiations.push((
                        format!("__dispatch_{fid}"),
                        "dispatch".to_owned(),
                        byte_offset,
                        None,
                    ));
                }
            }
            HelperSpec::PromiseReaction {
                kind,
                promise_ty,
                inner_ty,
                on_fulfilled,
                on_rejected,
                ret_ty,
            } => {
                let function = self.promise_reaction(
                    fid,
                    kind,
                    promise_ty,
                    inner_ty,
                    on_fulfilled,
                    on_rejected,
                    ret_ty,
                )?;
                self.plans.functions.insert(fid, function);
            }
            HelperSpec::TimerRunner {
                kind,
                timer_ty,
                cb_call,
                cb_ty,
                extra_arg_tys,
            } => {
                let runner =
                    self.timer_runner(fid, kind, timer_ty, cb_call, cb_ty, extra_arg_tys)?;
                self.plans.functions.insert(fid, runner);
            }
        }
        Ok(fid)
    }

    fn build_dispatcher(
        &mut self,
        id: FuncId,
        callee: Ty,
        span: DispatchArmSpan,
        args: Ty,
        receiver: Option<Ty>,
        result: Ty,
    ) -> Result<crate::engine::NativeFunction> {
        let args = self.types.tuple_slice(args);
        let mut params = vec![callee];
        params.extend(receiver);
        params.extend(args.iter().copied().filter(|&ty| !self.erases_argument(ty)));
        for &ty in &params {
            self.prepare_layouts(ty)?;
        }
        let result = if matches!(result, Ty::BOTTOM | Ty::NEVER) || self.is_zero_sized_func(result)
        {
            Ty::VOID
        } else {
            result
        };
        self.prepare_layouts(result)?;
        let mut is_async = false;
        let mut arms = Vec::new();
        for &(identity, instance) in &self.solved.dispatch_arms[span] {
            let view = self.index.view(identity.template);
            let target = &self.solved.instances[instance];
            is_async |= view.is_async;
            let count = if matches!(
                view.kind,
                TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
            ) {
                self.types.tuple_slice(target.args).len()
            } else if view.params.iter().any(|p| p.is_rest) {
                args.len()
            } else {
                view.params.len()
            };
            arms.push(crate::engine::DispatchArm {
                identity,
                target: target.emitted_func_id,
                receiver: view.uses_this && receiver.is_some(),
                arguments: args
                    .iter()
                    .take(count)
                    .filter(|&&ty| !self.erases_argument(ty))
                    .count(),
            });
        }
        Ok(crate::engine::NativeFunction {
            name: format!("__dispatch_{id}"),
            params: params
                .into_iter()
                .map(crate::engine::NativeParam::Value)
                .collect(),
            result,
            is_async,
            body: crate::engine::NativeBody::Dispatch {
                receiver: receiver.is_some(),
                arms,
            },
        })
    }
}
