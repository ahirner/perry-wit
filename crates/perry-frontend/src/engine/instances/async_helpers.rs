//! Emission of guest-settled promises (`new Promise`, `then`/`catch`/`finally`) and global timers.
use super::InstancePlanner;
use crate::engine::boundary::TemplateKind;
use crate::engine::solve::{CallTarget, Instance, ReactionKind};
use crate::engine::types::Ty;
use perry_hir::types::FuncId;
use perry_wit_diagnostics::Result;

impl<'h> InstancePlanner<'h, '_, '_> {
    pub(super) fn promise_handle_function(
        &mut self,
        inst: &Instance,
        kind: TemplateKind,
    ) -> Result<()> {
        use crate::engine::{NativeBody, NativeFunction, NativeParam};
        let identity = inst.identity();
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn promise_reaction(
        &mut self,
        fid: FuncId,
        kind: ReactionKind,
        promise: Ty,
        inner: Ty,
        fulfilled: Option<(CallTarget, Ty)>,
        rejected: Option<(CallTarget, Ty)>,
        result: Ty,
    ) -> Result<crate::engine::NativeFunction> {
        use crate::engine::{NativeBody, NativeFunction, NativeParam, Reaction};
        self.prepare_layouts(promise)?;
        let mut params = vec![NativeParam::Value(promise)];
        let mut bind = |(call, ty): (CallTarget, Ty)| {
            if call.needs_value() {
                self.prepare_layouts(ty)?;
                params.push(NativeParam::Value(ty));
            }
            self.prepare_call(call)?;
            Result::Ok(call)
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

    pub(super) fn prepare_call(&mut self, call: CallTarget) -> Result<()> {
        if let CallTarget::Dispatch { helper, .. } = call {
            self.helper_function(helper)?;
        }
        Ok(())
    }

    pub(super) fn timer_runner(
        &mut self,
        id: FuncId,
        kind: crate::engine::TimerKind,
        timer: Ty,
        cb_call: CallTarget,
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
        self.prepare_call(cb_call)?;
        Ok(NativeFunction {
            name: format!("__timer_{id}"),
            params,
            result: Ty::VOID,
            is_async: true,
            body: NativeBody::Timer {
                kind,
                callback: cb_call,
            },
        })
    }
}
