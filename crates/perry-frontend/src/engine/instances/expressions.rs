//! Instance facts attached to their original expression sites.
use super::*;
use crate::engine::solve::{SiteResolution, ValueCoercion};
use crate::engine::{CaptureOperand, EnvironmentField};

impl<'h> InstancePlanner<'h, '_> {
    pub(super) fn prepare_instance(&mut self, instance: InstanceId) -> Result<()> {
        let template = self.solved.instances[instance].template;
        for binding in self.index.bindings(template) {
            self.prepare_local(instance, binding)?;
        }
        for (source, site) in self
            .index
            .sites(template)
            .iter()
            .zip(&self.solved.inst_sites[instance])
        {
            self.prepare_site(instance, source.expr, *site)?;
        }
        Ok(())
    }

    fn prepare_site(
        &mut self,
        instance: InstanceId,
        expr: &'h Expr,
        site: SiteEntry,
    ) -> Result<()> {
        let owner = self.solved.body_owner(instance);
        match site.coercion {
            Some(FuncCoercion::Callback { target, inputs }) => {
                self.prepare_call(target)?;
                for &ty in self.types.tuple_slice(inputs) {
                    self.prepare_layouts(ty)?;
                }
            }
            Some(FuncCoercion::Value(ValueCoercion::Zero | ValueCoercion::Tag(..)))
                if is_pure_func_expr(expr) =>
            {
                return Ok(());
            }
            _ => {}
        }
        match site.resolution {
            Some(
                SiteResolution::BranchResult(ty)
                | SiteResolution::TypedArray(ty)
                | SiteResolution::CollectionNew(ty),
            ) => self.prepare_layouts(ty)?,
            Some(SiteResolution::Call { target, .. }) => self.prepare_call(target)?,
            Some(SiteResolution::Callback { .. }) => return Ok(()),
            Some(
                SiteResolution::PromiseReaction { helper, .. }
                | SiteResolution::TimerStart { helper },
            ) => {
                self.helper_function(helper)?;
            }
            Some(SiteResolution::PromiseNew {
                value_loc,
                executor,
                ..
            }) => {
                let executor =
                    executor.ok_or_else(|| Diagnostic::internal("unresolved promise executor"))?;
                self.prepare_layouts(self.solved.locations[value_loc])?;
                for handle in executor.handles.into_iter().flatten() {
                    self.environment(handle)?;
                }
                self.prepare_call(executor.call)?;
            }
            _ => {}
        }
        if let Expr::Closure { func_id, .. } = expr
            && let Some(tid) = self.index.template_of_func(*func_id)
            && self.is_env_template(tid)
        {
            let capture_owner = if self.index.template_captures(tid).is_empty() {
                CaptureOwner::None
            } else if self.solved.collapsed_closures.contains(tid.index()) {
                CaptureOwner::Template(self.solved.instances[instance].template)
            } else {
                CaptureOwner::Instance(instance)
            };
            let ident = FuncIdentity {
                template: tid,
                capture_owner,
            };
            let env = self.environment(ident)?;
            let operands = self
                .plans
                .environments
                .fields(env)
                .iter()
                .filter_map(|field| {
                    let &EnvironmentField::Capture {
                        local, ty, cell, ..
                    } = field
                    else {
                        return None;
                    };
                    Some(if cell.is_some() {
                        CaptureOperand::Cell(local)
                    } else {
                        self.capture_operand(
                            capture_owner,
                            CaptureOwner::Instance(instance),
                            local,
                            ty,
                        )
                    })
                })
                .collect();
            self.plans.closure_values.insert(
                (owner, *func_id),
                crate::engine::ClosureValue {
                    identity: ident,
                    operands,
                },
            );
        }
        Ok(())
    }
}
