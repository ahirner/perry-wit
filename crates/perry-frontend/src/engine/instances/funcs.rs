//! Closure environments, callback bindings, and finite function dispatch.
use super::InstancePlanner;
use crate::engine::EnvironmentField;
use crate::engine::bitset::BitSet;
use crate::engine::boundary::{LocalFlags, TemplateKind};
use crate::engine::solve::HelperSpec;
use crate::engine::types::{
    CaptureOwner, DispatchArmSpan, FuncIdentity, HelperId, Idx, TemplateId, Ty, TyKind,
};
use crate::hir_compat;
use perry_hir::types::{FuncId, LocalId};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};

impl<'h, 'a> InstancePlanner<'h, 'a, '_> {
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
        let loc = self.solved.capture_locs.get(
            self.index,
            &self.solved.instances,
            owner,
            self.index.local_idx(id)?,
        )?;
        Some(self.solved.locations[loc])
    }

    fn capture_fields(&mut self, ident: FuncIdentity) -> Result<Vec<EnvironmentField>> {
        let caps = self.index.template_captures(ident.template);
        let mut fields = Vec::with_capacity(caps.len());
        for &cap_id in caps {
            let cap_ty = match self.local_ty_in_owner(ident.capture_owner, cap_id) {
                Some(ty) if !matches!(ty, Ty::BOTTOM | Ty::OPEN) => ty,
                _ => {
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
            self.prepare_layouts(cap_ty)?;
            fields.push(EnvironmentField::Capture {
                local: cap_id,
                ty: cap_ty,
                cell: self.is_boxed_local(cap_id).then(|| self.cell_init(cap_id)),
            });
        }
        if fields.is_empty() {
            fields.push(EnvironmentField::Unit);
        }
        Ok(fields)
    }

    pub(super) fn environment(
        &mut self,
        ident: FuncIdentity,
    ) -> Result<crate::engine::types::EnvId> {
        use perry_mir::PlironResultExt;
        if let Some(id) = self.plans.environments.id(ident) {
            return Ok(id);
        }
        let (ctx, records) = &mut self.layouts;
        let record = records
            .fresh(ctx, perry_mir::RecordKind::Object, None)
            .to_diag(ctx)?;
        let id = self.plans.environments.reserve(ident, record);
        let fields = match self.index.view(ident.template).kind {
            TemplateKind::PromiseResolve(site) | TemplateKind::PromiseReject(site) => {
                let loc =
                    super::SiteEntry::promise(&self.solved.inst_sites, ident.capture_owner, site)?;
                let elem = self.solved.locations[loc];
                self.prepare_layouts(elem)?;
                vec![EnvironmentField::Settler(elem)]
            }
            _ => self.capture_fields(ident)?,
        };
        self.plans.environments.define(id, fields);
        Ok(id)
    }

    fn collect_env_idents_in(&self, ty: Ty, seen: &mut BitSet, out: &mut Vec<FuncIdentity>) {
        if !seen.insert(ty.index()) {
            return;
        }
        match self.types.kind(ty) {
            TyKind::Func(ident) => {
                if self.is_env_template(ident.template)
                    && self.plans.environments.id(ident).is_none()
                {
                    out.push(ident);
                }
            }
            TyKind::Array(i) | TyKind::Promise(i) => self.collect_env_idents_in(i, seen, out),
            TyKind::Cell { loc, .. } => {
                self.collect_env_idents_in(self.solved.locations[loc], seen, out);
            }
            TyKind::Tuple(s) | TyKind::Union(s) | TyKind::Generic { args: s, .. } => {
                for &m in self.types.ty_slice(s) {
                    self.collect_env_idents_in(m, seen, out);
                }
            }
            TyKind::Object { props, index } => {
                if let Some(i) = index {
                    self.collect_env_idents_in(i, seen, out);
                }
                for p in self.types.object_slice(props) {
                    self.collect_env_idents_in(p.ty, seen, out);
                }
            }
            _ => {}
        }
    }

    pub(super) fn prepare_layouts(&mut self, ty: Ty) -> Result<()> {
        let mut idents = Vec::new();
        self.collect_env_idents_in(ty, &mut BitSet::default(), &mut idents);
        for ident in idents {
            self.environment(ident)?;
        }
        Ok(())
    }

    pub(super) fn capture_operand(
        &self,
        owner: CaptureOwner,
        source: CaptureOwner,
        local: LocalId,
        to: Ty,
    ) -> crate::engine::CaptureOperand {
        use crate::engine::CaptureOperand;
        if matches!(owner, CaptureOwner::Template(_))
            && let Some(from) = self.local_ty_in_owner(source, local)
            && let Some(conversion) = crate::engine::solve::ValueCoercion::into_union(
                self.types,
                from,
                to,
                &self.index.capture_free_templates,
                &self.solved.escaping_env_templates,
            )
        {
            return CaptureOperand::Coerce(local, conversion);
        }
        CaptureOperand::Local(local)
    }

    pub(super) fn is_zero_sized_func(&self, ty: Ty) -> bool {
        self.types
            .is_zero_sized_func(ty, &self.index.capture_free_templates)
    }

    pub(super) fn is_env_template(&self, tid: TemplateId) -> bool {
        self.solved.escaping_env_templates.contains(tid.index())
    }

    pub(super) fn fresh_func_id(&mut self) -> Result<FuncId> {
        let next = self.next_func_id;
        self.next_func_id = next
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("function identifier limit"))?;
        Ok(next)
    }

    pub(super) fn helper_function(&mut self, id: HelperId) -> Result<FuncId> {
        if let Some(fid) = self.plans.helpers[id] {
            return Ok(fid);
        }
        let entry = self.solved.helpers[id];
        let (spec, byte_offset) = (entry.spec, entry.byte_offset);
        let fid = self.fresh_func_id()?;
        self.plans.helpers[id] = Some(fid);
        match spec {
            HelperSpec::Dispatcher {
                result,
                callee_ty,
                arms,
                arg_tys,
                recv_ty,
            } => {
                let dispatcher =
                    self.build_dispatcher(fid, callee_ty, arms, arg_tys, recv_ty, result)?;
                self.native.push((fid, dispatcher));
                if hir_compat::has_source_offset(byte_offset) {
                    self.locations.record_instantiation(
                        format!("__dispatch_{fid}"),
                        "dispatch".into(),
                        byte_offset,
                        None,
                    );
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
                self.native.push((fid, function));
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
                self.native.push((fid, runner));
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
        self.prepare_arguments(args);
        let slots = self.plans.arguments(args)?;
        let mut params = vec![callee];
        params.extend(receiver);
        params.extend(slots.iter().map(|&(_, ty)| ty));
        let result = if matches!(result, Ty::BOTTOM | Ty::NEVER) || self.is_zero_sized_func(result)
        {
            Ty::VOID
        } else {
            result
        };
        let is_async = self.solved.dispatch_arms[span]
            .iter()
            .any(|arm| self.index.view(arm.identity.template).is_async);
        for &ty in &params {
            self.prepare_layouts(ty)?;
        }
        self.prepare_layouts(result)?;
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
                arms: span,
                inputs: args,
            },
        })
    }
}
