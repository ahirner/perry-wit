//! Instance facts attached to their original expression sites.
use super::*;
use crate::engine::solve::SiteResolution;
use crate::engine::types::{CaptureConversionSpan, FuncIdentity, Pool};
use crate::engine::{ClosureValue, EnvironmentField, solve::ValueCoercion};

impl<'h> InstancePlanner<'h, '_, '_> {
    pub(super) fn prepare_bindings(&mut self, instance: InstanceId) -> Result<()> {
        let template = self.solved.instances[instance].template;
        for binding in self.index.bindings(template) {
            self.prepare_local(instance, binding)?;
        }
        Ok(())
    }

    pub(super) fn finalize_sites(&mut self) -> Result<()> {
        let mut rows = Vec::new();
        let mut conversions = Pool::default();
        for instance in self.solved.instances.ids() {
            let owner = self.solved.body_owner(instance);
            let locals = std::mem::take(&mut self.locals[instance]);
            if !self.solved.live_instances.contains(instance.index()) {
                if !locals.is_empty() {
                    self.plans.decisions.record_body(owner, [], locals);
                }
                continue;
            }
            for (&index, &site) in &self.solved.inst_sites[instance] {
                let mut site = site;
                match site.resolution {
                    Some(SiteResolution::Closure(ident)) => {
                        let elided = site.conversion().is_some_and(|c| !c.reads_operand());
                        site.resolution = if self.is_env_template(ident.template) && !elided {
                            Some(SiteResolution::Environment(self.closure_value(
                                instance,
                                ident,
                                &mut conversions,
                            )?))
                        } else {
                            None
                        };
                    }
                    Some(SiteResolution::Cell(_)) => site.resolution = None,
                    Some(SiteResolution::PromiseNew { executor, .. }) => {
                        let executor = executor
                            .ok_or_else(|| Diagnostic::internal("unresolved promise executor"))?;
                        for handle in executor.handles.into_iter().flatten() {
                            self.environment(handle)?;
                        }
                    }
                    _ => {}
                }
                if site.erased || site.coercion.is_some() || site.resolution.is_some() {
                    rows.push((index, site));
                }
            }
            self.plans
                .decisions
                .record_body(owner, rows.drain(..), locals);
        }
        self.plans.capture_conversions = conversions;
        Ok(())
    }

    fn closure_value(
        &mut self,
        instance: InstanceId,
        ident: FuncIdentity,
        pool: &mut Pool<CaptureConversionSpan, (LocalId, ValueCoercion)>,
    ) -> Result<ClosureValue> {
        let layout = self.environment(ident)?;
        let conversions = if matches!(ident.capture_owner, CaptureOwner::Template(..)) {
            pool.extend(
                self.plans.environment_fields[layout.fields]
                    .iter()
                    .filter_map(|field| {
                        let &EnvironmentField::Capture {
                            local,
                            ty,
                            cell: None,
                        } = field
                        else {
                            return None;
                        };
                        let loc = self.local_location(
                            CaptureOwner::Instance(instance),
                            self.index.local_idx(local)?,
                        )?;
                        let conversion = ValueCoercion::into_union(
                            self.types,
                            self.solved.locations[loc],
                            ty,
                            &self.index.capture_free_templates,
                            &self.solved.escaping_env_templates,
                        )?;
                        Some((local, conversion))
                    }),
            )
        } else {
            Default::default()
        };
        Ok(ClosureValue {
            identity: ident,
            conversions,
        })
    }
}
