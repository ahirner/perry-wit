//! Guest promise creation, settlement, and reaction handlers.
use super::*;
use crate::engine::{Reaction, solve::CallTarget, types::Ty};

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn settle_body(
        &mut self,
        ctx: &mut Context,
        kind: perry_mir::SettleKind,
        args: &[Value],
    ) -> Result<()> {
        let (&env, values) = args
            .split_first()
            .ok_or_else(|| internal!("missing promise environment"))?;
        let promise = self.capture_field(ctx, env, 0)?;
        let settled = perry_mir::typed::<PromiseType>(ctx, promise.get_type(ctx))
            .ok_or_else(|| internal!("expected a captured promise"))?
            .deref(ctx)
            .value();
        let expected = (kind == perry_mir::SettleKind::Fulfill).then_some(settled);
        let value = match values {
            [value] => *value,
            [] => self.absent_literal(ctx, false, expected)?,
            _ => bail_internal!("too many promise settlement arguments"),
        };
        let value = match expected {
            Some(ty) => self.fit(ctx, value, ty)?,
            None => value,
        };
        self.emit(ctx, |ctx| PromiseSettleOp::new(ctx, kind, promise, value));
        self.emit(ctx, |ctx| ReturnOp::new(ctx, Vec::new()));
        Ok(())
    }

    pub(super) fn new_promise(
        &mut self,
        ctx: &mut Context,
        value_loc: crate::engine::types::LocId,
        executor: crate::engine::solve::PromiseExecutor,
        args: &'s [Expr],
    ) -> Result<Value> {
        let settled = self.shapes.specialization.locations[value_loc];
        let handles = executor.handles;
        let executor = executor.call;
        let mut value = None;
        for (i, arg) in args.iter().enumerate() {
            if i == 0 && executor.needs_value() {
                value = Some(self.expression(ctx, arg)?);
            } else {
                self.discard_function_operand(ctx, arg)?;
            }
        }
        if matches!(executor, CallTarget::ThrowTypeError) {
            return Ok(self.raise_non_callable(ctx));
        }
        if let Some(value) = value
            && absence(ctx, value.get_type(ctx)).is_some()
        {
            let callable = self.truthy(ctx, value)?;
            let valid = self.new_block(ctx, "executor_callable");
            let invalid = self.new_block(ctx, "executor_absent");
            self.branch(ctx, callable, valid, invalid);
            self.switch_to(invalid);
            self.raise_error(
                ctx,
                ErrorKind::TypeError,
                "Promise executor is not a function",
            );
            self.switch_to(valid);
        }
        let settled = self
            .shapes
            .resolved_binding(if settled == Ty::BOTTOM {
                Ty::VOID
            } else {
                settled
            })?
            .carrier;
        let promise = self.emit_value(ctx, |ctx| PromisePendingOp::new(ctx, settled));
        let mut inputs = Vec::new();
        for handle in handles.into_iter().flatten() {
            let record = self.shapes.environment_record(handle)?;
            let capture = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, vec![promise]));
            inputs.push(self.function_value(ctx, handle, Some(capture), None)?);
        }
        let handler = self.catch_handler(ctx, "executor_threw");
        self.handlers.push(handler);
        self.invoke_callback(ctx, executor, value, None, &inputs)?;
        self.handlers.pop();
        let done = self.new_block(ctx, "executor_done");
        self.jump(ctx, done);
        self.switch_to(handler.block);
        let reason = self.catch_exception(ctx, handler.status)?;
        self.emit(ctx, |ctx| {
            PromiseSettleOp::new(ctx, perry_mir::SettleKind::Reject, promise, reason)
        });
        self.jump(ctx, done);
        self.switch_to(done);
        Ok(promise)
    }

    pub(super) fn return_callback(
        &mut self,
        ctx: &mut Context,
        value: Option<Value>,
        discard: bool,
    ) -> Result<()> {
        let value = value.map(|v| self.adopt(ctx, v)).transpose()?;
        let values = match (self.result, value) {
            (Some(binding), Some(value)) if !discard => {
                vec![self.fit_binding(ctx, value, binding)?]
            }
            _ => self.implicit_results(ctx),
        };
        self.emit(ctx, |ctx| ReturnOp::new(ctx, values));
        Ok(())
    }

    fn adopt(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if self.is_async && value.get_type(ctx).deref(ctx).is::<PromiseType>() {
            let op = AwaitOp::new(ctx, value).to_diag(ctx)?;
            Ok(self.append_value(ctx, &op))
        } else {
            Ok(value)
        }
    }

    fn if_callable(
        &mut self,
        ctx: &mut Context,
        value: Option<Value>,
    ) -> Result<Option<Ptr<BasicBlock>>> {
        let Some(value) = value.filter(|v| absence(ctx, v.get_type(ctx)).is_some()) else {
            return Ok(None);
        };
        let test = self.truthy(ctx, value)?;
        let invoke = self.new_block(ctx, "handler_present");
        let skip = self.new_block(ctx, "handler_absent");
        self.branch(ctx, test, invoke, skip);
        self.switch_to(invoke);
        Ok(Some(skip))
    }

    fn finalizer(
        &mut self,
        ctx: &mut Context,
        call: CallTarget,
        value: Option<Value>,
    ) -> Result<()> {
        let skip = self.if_callable(ctx, value)?;
        if let Some(result) = self.invoke_callback(ctx, call, value, None, &[])? {
            self.adopt(ctx, result)?;
        }
        if let Some(skip) = skip {
            self.jump(ctx, skip);
            self.switch_to(skip);
        }
        Ok(())
    }

    pub(super) fn reaction_body(
        &mut self,
        ctx: &mut Context,
        reaction: &Reaction,
        discard: bool,
        args: &[Value],
    ) -> Result<()> {
        let (&promise, mut args) = args
            .split_first()
            .ok_or_else(|| internal!("missing source promise"))?;
        let (fulfilled, rejected) = match *reaction {
            Reaction::Chain {
                fulfilled,
                rejected,
                ..
            } => (fulfilled, rejected),
            Reaction::Finally(callback) => (Some(callback), None),
        };
        let f = fulfilled
            .map(|cb| cb.take_operand(&mut args))
            .transpose()?
            .flatten();
        let g = rejected
            .map(|cb| cb.take_operand(&mut args))
            .transpose()?
            .flatten();
        let handler = (rejected.is_some() || matches!(reaction, Reaction::Finally(_)))
            .then(|| self.catch_handler(ctx, "promise_rejected"));
        self.handlers.extend(handler);
        let op = AwaitOp::new(ctx, promise).to_diag(ctx)?;
        let settled = self.append_value(ctx, &op);
        if handler.is_some() {
            self.handlers.pop();
        }
        match *reaction {
            Reaction::Finally(callback) => {
                self.finalizer(ctx, callback, f)?;
                self.return_callback(ctx, Some(settled), discard)?;
            }
            Reaction::Chain {
                fulfilled: Some(callback),
                ..
            } => {
                let skip = self.if_callable(ctx, f)?;
                let result = self.invoke_callback(ctx, callback, f, None, &[settled])?;
                self.return_callback(ctx, result, discard)?;
                if let Some(skip) = skip {
                    self.switch_to(skip);
                    self.return_callback(ctx, Some(settled), discard)?;
                }
            }
            Reaction::Chain {
                rejects_only: true,
                rejected: Some(_),
                ..
            } if !discard => self.raise_error(ctx, ErrorKind::Any, "unreachable"),
            Reaction::Chain { .. } => self.return_callback(ctx, Some(settled), discard)?,
        }
        if let Some(handler) = handler {
            self.switch_to(handler.block);
            let reason = self.catch_exception(ctx, handler.status)?;
            if let Reaction::Finally(callback) = *reaction {
                self.finalizer(ctx, callback, f)?;
            } else if let Some(callback) = rejected {
                let skip = self.if_callable(ctx, g)?;
                let result = self.invoke_callback(ctx, callback, g, None, &[reason])?;
                self.return_callback(ctx, result, discard)?;
                let Some(skip) = skip else { return Ok(()) };
                self.switch_to(skip);
            }
            let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, reason));
            self.propagate(ctx, status);
        }
        Ok(())
    }
}
