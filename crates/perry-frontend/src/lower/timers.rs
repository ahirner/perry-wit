//! Timer allocation, cancellation, and asynchronous callback execution.
use super::*;
use crate::engine::{TimerKind, solve::CallTarget};

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn start_timer(
        &mut self,
        ctx: &mut Context,
        id: FuncId,
        args: &'s [Expr],
    ) -> Result<Value> {
        let signature = self.signature(id)?;
        let (handle, params) = signature
            .params
            .split_first()
            .ok_or_else(|| internal!("missing timer handle parameter"))?;
        let Some(Callee::Native(runner, _)) = self.signatures.callee(id) else {
            bail_internal!("expected a timer runner")
        };
        let crate::engine::NativeBody::Timer { kind, callback } = runner.body else {
            bail_internal!("expected a timer runner")
        };
        let (callback_arg, remaining) = args
            .split_first()
            .ok_or_else(|| internal!("missing timer callback"))?;
        let retained = if callback.needs_value() {
            args
        } else {
            self.discard_function_operand(ctx, callback_arg)?;
            remaining
        };
        let default_delay = kind != TimerKind::Immediate && remaining.is_empty();
        ensure_internal!(
            retained.len() + usize::from(default_delay) == params.len(),
            "timer runner argument count mismatch"
        );
        let mut operands = Vec::with_capacity(signature.params.len());
        for (param, arg) in params.iter().zip(retained) {
            operands.push(self.bind_fixed_arg(ctx, param, Some(arg))?);
        }
        if default_delay {
            operands.push(self.number(ctx, 0.0));
        }
        let controller = self.intrinsic(ctx, Intrinsic::AbortNew, vec![]);
        let cancelled = self.boolean(ctx, false);
        let record = perry_mir::typed::<RecordType>(ctx, handle.binding.carrier)
            .ok_or_else(|| internal!("expected timer record"))?;
        let timer = self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, record, vec![cancelled, controller])
        });
        operands.insert(0, timer);
        self.emit_signature_call(ctx, signature, operands)?;
        Ok(timer)
    }

    pub(super) fn clear_timer(&mut self, ctx: &mut Context, operand: Value) -> Result<()> {
        let ty = self
            .shapes
            .resolved_binding(crate::engine::types::Ty::TIMER)?
            .carrier;
        let done = self.new_block(ctx, "timer_cleared");
        if absence(ctx, operand.get_type(ctx)).is_some() {
            let present = self.truthy(ctx, operand)?;
            let cancel = self.new_block(ctx, "cancel_timer");
            self.branch(ctx, present, cancel, done);
            self.switch_to(cancel);
        }
        let timer = self.defined(ctx, operand)?;
        let timer = self.fit(ctx, timer, ty)?;
        let cancelled = self.boolean(ctx, true);
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, timer, "cancelled".into(), cancelled)
        });
        let field = FieldGetOp::new(ctx, timer, "ctrl".into()).to_diag(ctx)?;
        let controller = self.append_value(ctx, &field);
        self.intrinsic_void(ctx, Intrinsic::AbortAbort, vec![controller]);
        self.jump(ctx, done);
        self.switch_to(done);
        Ok(())
    }

    pub(super) fn timer_body(
        &mut self,
        ctx: &mut Context,
        kind: TimerKind,
        callback: CallTarget,
        args: &[Value],
    ) -> Result<()> {
        let (&timer, args) = args
            .split_first()
            .ok_or_else(|| internal!("missing timer handle"))?;
        let mut args = args;
        let callback_value = callback.take_operand(&mut args)?;
        let (delay, args) = if kind == TimerKind::Immediate {
            (self.number(ctx, 0.0), args)
        } else {
            let (&delay, args) = args
                .split_first()
                .ok_or_else(|| internal!("missing timer delay"))?;
            (delay, args)
        };
        let done = self.new_block(ctx, "timer_done");
        let wait = self.new_block(ctx, "timer_wait");
        self.jump(ctx, wait);
        self.switch_to(wait);
        if kind == TimerKind::Interval {
            let active = self.new_block(ctx, "timer_active");
            let cancelled = self.record_field(ctx, timer, "cancelled")?;
            self.branch(ctx, cancelled, done, active);
            self.switch_to(active);
        }
        let handler = self.catch_handler(ctx, "timer_cancelled");
        self.handlers.push(handler);
        let signal = self.record_field(ctx, timer, "ctrl")?;
        let undef = self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, FP64Type::get(ctx).into()));
        let pending = self.emit_value(ctx, |ctx| TimerOp::new(ctx, delay, undef, Some(signal)));
        let op = AwaitOp::new(ctx, pending).to_diag(ctx)?;
        self.append_value(ctx, &op);
        self.handlers.pop();
        let run = self.new_block(ctx, "timer_callback");
        let cancelled = self.record_field(ctx, timer, "cancelled")?;
        self.branch(ctx, cancelled, done, run);
        self.switch_to(run);
        self.invoke_callback(ctx, callback, callback_value, Some(timer), args)?;
        self.jump(
            ctx,
            if kind == TimerKind::Interval {
                wait
            } else {
                done
            },
        );
        self.switch_to(handler.block);
        self.catch_exception(ctx, handler.status)?;
        self.jump(ctx, done);
        self.switch_to(done);
        let result = self.implicit_results(ctx);
        self.emit(ctx, |ctx| ReturnOp::new(ctx, result));
        Ok(())
    }
}
