//! Emission of guest-settled promises (`new Promise`, `then`/`catch`/`finally`) and global timers.
use super::InstancePlanner;
use crate::engine::PromiseConstructor;
use crate::engine::boundary::TemplateKind;
use crate::engine::solve::{CallbackCall, Instance, ReactionKind};
use crate::engine::types::{CaptureOwner, FuncIdentity, LocId, TemplateId, Ty};
use perry_hir::types::FuncId;
use perry_wit_diagnostics::Result;

impl<'h> InstancePlanner<'h, '_> {
    pub(super) fn promise_handle_function(
        &mut self,
        inst: &Instance,
        kind: TemplateKind,
    ) -> Result<()> {
        use crate::engine::{NativeBody, NativeFunction, NativeParam};
        let identity = FuncIdentity {
            template: inst.template,
            capture_owner: inst.capture_owner,
        };
        self.environment(identity)?;
        let mut params = vec![NativeParam::Environment(identity)];
        if let [ty] = self.types.tuple_slice(inst.args) {
            self.prepare_layouts(*ty)?;
            params.push(NativeParam::Value(*ty));
        }
        let settle = if matches!(kind, TemplateKind::PromiseResolve(_)) {
            perry_mir::SettleKind::Fulfill
        } else {
            perry_mir::SettleKind::Reject
        };
        self.plans.functions.insert(
            inst.emitted_func_id,
            NativeFunction {
                name: inst.emitted_name.clone(),
                params,
                result: Ty::VOID,
                is_async: false,
                body: NativeBody::Settle(settle),
            },
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn promise_constructor(
        &mut self,
        instance: crate::engine::types::InstanceId,
        value_loc: LocId,
        resolve: TemplateId,
        reject: TemplateId,
        executor: CallbackCall,
        pass_resolve: bool,
        pass_reject: bool,
    ) -> Result<PromiseConstructor> {
        let settled = self.solved.locations[value_loc];
        self.prepare_layouts(settled)?;
        let owner = CaptureOwner::Instance(instance);
        let handles = [
            pass_resolve.then_some(resolve),
            pass_reject.then_some(reject),
        ]
        .map(|template| {
            template.map(|template| FuncIdentity {
                template,
                capture_owner: owner,
            })
        });
        for &handle in handles.iter().flatten() {
            self.environment(handle)?;
        }
        let executor = self.native_callback(executor)?;
        Ok(PromiseConstructor {
            settled,
            executor,
            handles,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn promise_reaction(
        &mut self,
        fid: FuncId,
        kind: ReactionKind,
        promise: Ty,
        inner: Ty,
        fulfilled: Option<(CallbackCall, Ty)>,
        rejected: Option<(CallbackCall, Ty)>,
        result: Ty,
    ) -> Result<crate::engine::NativeFunction> {
        use crate::engine::{NativeBody, NativeFunction, NativeParam, Reaction};
        self.prepare_layouts(promise)?;
        let mut params = vec![NativeParam::Value(promise)];
        let mut bind = |(call, ty): (CallbackCall, Ty)| {
            if call.needs_value() {
                self.prepare_layouts(ty)?;
                params.push(NativeParam::Value(ty));
            }
            self.native_callback(call)
        };
        let fulfilled = fulfilled.map(&mut bind).transpose()?;
        let rejected = rejected.map(bind).transpose()?;
        let result = if result == Ty::BOTTOM {
            Ty::VOID
        } else {
            result
        };
        self.prepare_layouts(result)?;
        let reaction = match (kind, fulfilled) {
            (ReactionKind::Finally, Some(call)) => Reaction::Finally(call),
            _ => Reaction::Chain {
                fulfilled,
                rejected,
                rejects_only: matches!(inner, Ty::BOTTOM | Ty::NEVER),
            },
        };
        let label = match kind {
            ReactionKind::Then => "then",
            ReactionKind::Catch => "catch",
            ReactionKind::Finally => "finally",
        };
        Ok(NativeFunction {
            name: format!("__{label}_{fid}"),
            params,
            result,
            is_async: true,
            body: NativeBody::Reaction {
                reaction,
                returns_void: result == Ty::VOID,
            },
        })
    }

    pub(super) fn native_callback(
        &mut self,
        call: CallbackCall,
    ) -> Result<crate::engine::NativeCallback> {
        use crate::engine::{CallbackOperand, NativeCallback};
        Ok(match call {
            CallbackCall::ThrowTypeError => NativeCallback::ThrowTypeError,
            CallbackCall::Direct {
                target,
                pass_env,
                pass_receiver,
            } => NativeCallback::Call {
                target: self.solved.instances[target].emitted_func_id,
                operand: if pass_env {
                    CallbackOperand::Environment
                } else {
                    CallbackOperand::None
                },
                receiver: pass_receiver,
            },
            CallbackCall::Dispatch { helper, needs_recv } => NativeCallback::Call {
                target: self.helper_function(helper)?,
                operand: CallbackOperand::Callee,
                receiver: needs_recv,
            },
        })
    }

    pub(super) fn timer_runner(
        &mut self,
        id: FuncId,
        kind: crate::engine::TimerKind,
        timer: Ty,
        cb_call: CallbackCall,
        callback_type: Ty,
        extra: Ty,
    ) -> Result<crate::engine::NativeFunction> {
        use crate::engine::{NativeBody, NativeFunction, NativeParam, TimerKind};
        let mut params = vec![NativeParam::Value(timer)];
        if cb_call.needs_value() {
            self.prepare_layouts(callback_type)?;
            params.push(NativeParam::Value(callback_type));
        }
        if kind != TimerKind::Immediate {
            params.push(NativeParam::Value(Ty::NUMBER));
        }
        for &ty in self.types.tuple_slice(extra) {
            self.prepare_layouts(ty)?;
            params.push(NativeParam::Value(ty));
        }
        Ok(NativeFunction {
            name: format!("__timer_{id}"),
            params,
            result: Ty::VOID,
            is_async: true,
            body: NativeBody::Timer {
                kind,
                callback: self.native_callback(cb_call)?,
            },
        })
    }
}
