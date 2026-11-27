//! Closure environments, callback bindings, and finite function dispatch.
use super::InstancePlanner;
use crate::engine::EnvironmentField;
use crate::engine::boundary::{LocalFlags, TemplateKind};
use crate::engine::types::{CaptureOwner, FuncIdentity, Idx, LocalIdx, TemplateId, Ty};
use perry_hir::types::FuncId;
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};

impl<'h, 'a> InstancePlanner<'h, 'a, '_> {
    pub(super) fn cell_init(&self, local: LocalIdx) -> Option<crate::engine::CellInit> {
        use crate::engine::CellInit;
        let flags = self.index.locals[local].flags;
        self.solved.boxed_locals.contains(local.index()).then(|| {
            if flags.contains(LocalFlags::FORWARD_CAPTURED) {
                CellInit::Hoisted
            } else if flags.contains(LocalFlags::DEFERRED) {
                CellInit::Deferred
            } else {
                CellInit::AtDeclaration
            }
        })
    }

    pub(super) fn local_location(
        &self,
        owner: CaptureOwner,
        local: LocalIdx,
    ) -> Option<super::LocId> {
        owner.location(
            &self.solved.instances,
            self.index.locals[local],
            self.solved.collapsed_locals[local],
        )
    }

    fn capture_fields(&self, ident: FuncIdentity) -> Result<Vec<EnvironmentField>> {
        let caps = self.index.template_captures(ident.template);
        let mut fields = Vec::with_capacity(caps.len());
        for &cap_id in caps {
            let local = self.index.local_idx(cap_id);
            let cap_ty = match local
                .and_then(|i| self.local_location(ident.capture_owner, i))
                .map(|loc| self.solved.locations[loc])
            {
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

            fields.push(EnvironmentField::Capture {
                local: cap_id,
                ty: cap_ty,
                cell: local.and_then(|i| self.cell_init(i)),
            });
        }
        if self.index.view(ident.template).captures_this
            && let Some(receiver) = ident.capture_owner.receiver(&self.solved.instances)
        {
            let ty = match receiver {
                super::Receiver::Class(id) => self.solved.class_insts[id].ty,
                super::Receiver::Object(ty) => ty,
            };
            fields.push(EnvironmentField::Receiver(ty));
        }
        if fields.is_empty() {
            fields.push(EnvironmentField::Unit);
        }
        Ok(fields)
    }

    pub(super) fn environment(
        &mut self,
        ident: FuncIdentity,
    ) -> Result<crate::engine::Environment> {
        use perry_mir::PlironResultExt;
        if let Some(layout) = self.plans.environments.get(&ident) {
            return Ok(*layout);
        }
        let fields = match self.index.view(ident.template).kind {
            TemplateKind::PromiseResolve(site) | TemplateKind::PromiseReject(site) => {
                let loc =
                    super::SiteEntry::promise(&self.solved.inst_sites, ident.capture_owner, site)?;
                let elem = self.solved.locations[loc];

                vec![EnvironmentField::Settler(elem)]
            }
            _ => self.capture_fields(ident)?,
        };
        let (ctx, records) = &mut self.layouts;
        let record = records
            .fresh(ctx, perry_mir::RecordKind::Object, None)
            .to_diag(ctx)?;
        let layout = crate::engine::Environment {
            record,
            fields: self.plans.environment_fields.extend(fields),
        };
        self.plans.environments.insert(ident, layout);
        Ok(layout)
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
}
