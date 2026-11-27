//! Timer allocation, cancellation, and asynchronous callback execution.
use super::*;
use crate::engine::{TimerSchedule, solve::CallTarget};
use crate::lower::calls::CallArguments::Values;

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn start_timer(
        &mut self,
        ctx: &mut Context,
        id: FuncId,
        args: &'s [Expr],
    ) -> Result<Value> {
        let signature = self.signature(id)?;
        let handle = signature
            .params
            .first()
            .ok_or_else(|| internal!("missing timer handle parameter"))?;
        let mut operands = self.bind_arguments(
            ctx,
            signature,
            1,
            std::iter::empty(),
            Arguments::Plain(args),
        )?;
        let controller = self.intrinsic(ctx, Intrinsic::AbortNew, vec![]);
        let cancelled = self.boolean(ctx, false);
        let record = perry_mir::typed::<RecordType>(ctx, handle.binding.carrier)
            .ok_or_else(|| internal!("expected timer record"))?;
        let timer = self.emit_value(AllocOp::new(ctx, record, vec![cancelled, controller]), ctx);
        operands.insert(0, timer);
        self.emit_signature_call(ctx, signature, operands, None)?;
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
        self.emit(
            FieldSetOp::new(ctx, timer, "cancelled".into(), cancelled),
            ctx,
        );
        let field = FieldGetOp::new(ctx, timer, "ctrl".into()).to_diag(ctx)?;
        let controller = self.emit_value(field, ctx);
        self.intrinsic_void(ctx, Intrinsic::AbortAbort, vec![controller]);
        self.jump(ctx, done);
        self.switch_to(done);
        Ok(())
    }

    pub(super) fn timer_body(
        &mut self,
        ctx: &mut Context,
        schedule: TimerSchedule,
        callback: CallTarget,
        args: &[Value],
    ) -> Result<()> {
        let (&timer, args) = args
            .split_first()
            .ok_or_else(|| internal!("missing timer handle"))?;
        let mut args = args;
        let callback_value = callback.take_operand(&mut args)?;
        let (delay, args) = if !schedule.has_delay {
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
        if schedule.repeats {
            let active = self.new_block(ctx, "timer_active");
            let cancelled = self.record_field(ctx, timer, "cancelled")?;
            self.branch(ctx, cancelled, done, active);
            self.switch_to(active);
        }
        let handler = self.catch_handler(ctx);
        self.with_handler(ctx, Some(handler), None, |b, ctx| {
            let signal = b.record_field(ctx, timer, "ctrl")?;
            let undef = b.emit_value(UndefinedOp::new(ctx, FP64Type::get(ctx).into()), ctx);
            let pending = b.emit_value(TimerOp::new(ctx, delay, undef, Some(signal)), ctx);
            let op = AwaitOp::new(ctx, pending).to_diag(ctx)?;
            b.emit_value(op, ctx);
            Ok(())
        })?;
        let run = self.new_block(ctx, "timer_callback");
        let cancelled = self.record_field(ctx, timer, "cancelled")?;
        self.branch(ctx, cancelled, done, run);
        self.switch_to(run);
        callback.invoke(self, ctx, callback_value, Some(timer), Values(args))?;
        self.jump(ctx, if schedule.repeats { wait } else { done });
        self.catch_exception(ctx, handler);
        self.jump(ctx, done);
        self.switch_to(done);
        let result = self.implicit_results(ctx);
        self.emit(ReturnOp::new(ctx, result), ctx);
        Ok(())
    }
}
