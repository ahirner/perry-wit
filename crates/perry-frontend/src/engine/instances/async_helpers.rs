//! Signatures for promise handles and specialized runtime helpers.
use super::InstancePlanner;
use crate::engine::boundary::TemplateKind;
use crate::engine::solve::{HelperSpec, Instance, ReactionKind};
use crate::engine::types::{HelperId, Ty};
use crate::engine::{NativeBody, NativeFunction, NativeParam};
use perry_wit_diagnostics::Result;

impl<'h> InstancePlanner<'h, '_, '_> {
    pub(super) fn promise_handle_function(
        &mut self,
        inst: &Instance,
        kind: TemplateKind,
    ) -> Result<()> {
        let identity = inst.identity();
        self.environment(identity)?;
        let mut params = vec![NativeParam::Environment(identity)];
        if let [ty] = self.types.tuple_slice(inst.args) {
            params.push(NativeParam::Value(*ty));
        }
        let settle = if matches!(kind, TemplateKind::PromiseResolve(_)) {
            perry_mir::SettleKind::Fulfill
        } else {
            perry_mir::SettleKind::Reject
        };
        self.native.push((
            inst.emitted_func_id,
            NativeFunction {
                name: inst.emitted_name.clone(),
                params,
                result: Ty::VOID,
                is_async: false,
                body: NativeBody::Settle(settle),
            },
        ));
        Ok(())
    }

    pub(super) fn prepare_helper(&mut self, id: HelperId) -> Result<()> {
        let entry = self.solved.helpers[id];
        let fid = self.fresh_func_id()?;
        self.plans.targets.helpers[id] = Some(fid);
        let (label, params, result, is_async) = match entry.spec {
            HelperSpec::Dispatcher {
                result,
                callee_ty,
                arms,
                arg_tys,
                recv_ty,
            } => {
                let args = self
                    .types
                    .runtime_arguments(arg_tys, &self.solved.escaping_env_templates);
                let params = std::iter::once(callee_ty)
                    .chain(recv_ty)
                    .map(NativeParam::Value)
                    .chain(args.map(|(index, ty)| NativeParam::Argument {
                        position: 1 + usize::from(recv_ty.is_some()) + index,
                        ty,
                    }))
                    .collect();
                let erased =
                    matches!(result, Ty::BOTTOM | Ty::NEVER) || self.is_zero_sized_func(result);
                let is_async = self.solved.dispatch_arms[arms]
                    .iter()
                    .any(|arm| self.index.view(arm.identity.template).is_async);
                if crate::hir_compat::has_source_offset(entry.byte_offset) {
                    self.locations.record_instantiation(
                        format!("__dispatch_{fid}"),
                        "dispatch".into(),
                        entry.byte_offset,
                        None,
                    );
                }
                (
                    "dispatch",
                    params,
                    if erased { Ty::VOID } else { result },
                    is_async,
                )
            }
            HelperSpec::PromiseReaction(reaction) => {
                let mut params = vec![NativeParam::Value(reaction.promise_ty)];
                for (index, callback) in reaction.arguments().into_iter().enumerate() {
                    if let Some((_, ty)) = callback.filter(|(call, _)| call.needs_value()) {
                        params.push(NativeParam::Argument {
                            position: index + 1,
                            ty,
                        });
                    }
                }
                let label = match reaction.kind {
                    ReactionKind::Then => "then",
                    ReactionKind::Catch => "catch",
                    ReactionKind::Finally => "finally",
                };
                (label, params, reaction.result(), true)
            }
            HelperSpec::TimerRunner {
                cb_call, inputs, ..
            } => {
                let mut params = vec![NativeParam::Value(Ty::TIMER)];
                params.extend(
                    self.types
                        .tuple_slice(inputs)
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| *index != 0 || cb_call.needs_value())
                        .map(|(index, &ty)| NativeParam::Argument {
                            position: index + 1,
                            ty,
                        }),
                );
                ("timer", params, Ty::VOID, true)
            }
        };
        self.native.push((
            fid,
            NativeFunction {
                name: format!("__{label}_{fid}"),
                params,
                result,
                is_async,
                body: NativeBody::Helper(entry.spec),
            },
        ));
        Ok(())
    }
}
