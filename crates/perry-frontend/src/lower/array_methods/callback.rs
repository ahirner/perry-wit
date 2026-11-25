use super::*;
use crate::lower::callable::Callback;
use crate::lower::inline::InlineResult;

impl<'s> FunctionBuilder<'s> {
    /// Apply an array callback (a named function or an inline closure) to `arguments`. An
    /// inline closure's body lowers in place, with its `return` statements branching to a `done` block.
    pub(in crate::lower) fn array_callback(
        &mut self,
        ctx: &mut Context,
        callback: Callback<'s>,
        arguments: &[Value],
    ) -> Result<Value> {
        self.inline_callback(ctx, callback, arguments, None)
    }

    /// Apply a callback whose result the caller discards, as `forEach` does.
    pub(in crate::lower) fn effect_callback(
        &mut self,
        ctx: &mut Context,
        callback: Callback<'s>,
        arguments: &[Value],
    ) -> Result<()> {
        self.inline_callback(ctx, callback, arguments, Some(InlineResult::Void))
            .map(|_| ())
    }

    /// Apply `callback` to `arguments`. An inline closure stores the `expected` result, or else
    /// the result type the frontend inferred for it.
    pub(in crate::lower) fn inline_callback(
        &mut self,
        ctx: &mut Context,
        callback: Callback<'s>,
        arguments: &[Value],
        expected: Option<InlineResult>,
    ) -> Result<Value> {
        let callback = match callback {
            Callback::Source(expr) => expr,
            Callback::Bound(binding, callee) => {
                let mut inputs = Vec::new();
                for (index, ty) in binding.params.iter().enumerate() {
                    let Some(ty) = ty else { continue };
                    let ty = self.shapes.resolved_binding(ctx, *ty)?.carrier;
                    let value = match arguments.get(index) {
                        Some(&value) => self.fit(ctx, value, ty)?,
                        None => self.absent_literal(ctx, false, Some(ty))?,
                    };
                    inputs.push(value);
                }
                let result =
                    self.invoke_callback(ctx, binding.call, Some(callee), None, &inputs)?;
                return match (result, expected) {
                    (Some(value), Some(InlineResult::Declared(ty))) => self.fit(ctx, value, ty),
                    (Some(value), _) => Ok(value),
                    (None, _) => self.absent_literal(ctx, false, None),
                };
            }
        };
        if let Expr::FuncRef(id) = callback {
            let signature = self.signature(*id)?;
            ensure_unsupported!(
                !signature.is_async && signature.params.iter().all(|p| !p.is_rest),
                "array callbacks require synchronous functions without rest parameters"
            );
            let values = signature
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| match arguments.get(index) {
                    Some(&value) => self.fit(ctx, value, param.binding.carrier),
                    None => self.bind_fixed_arg(ctx, param, None),
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(match self.emit_signature_call(ctx, signature, values)? {
                Some(value) => value,
                None => {
                    let ty = FP64Type::get(ctx).into();
                    self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
                }
            });
        }
        let Expr::Closure {
            func_id,
            params,
            body,
            is_async,
            is_generator,
            return_type,
            ..
        } = callback
        else {
            bail_unsupported!("array callbacks require an inline closure or named function")
        };
        ensure_unsupported!(
            !is_async && !is_generator && params.iter().all(|p| !p.is_rest),
            "array callbacks must be synchronous; rest parameters are not supported"
        );
        let result_type = match expected {
            Some(result) => result,
            None => self
                .shapes
                .closure_result(ctx, *func_id, return_type)?
                .map_or(InlineResult::Void, InlineResult::Declared),
        };
        let slot = self.with_inline_scope(ctx, *func_id, self.this_slot, |this, ctx| {
            for (index, param) in params.iter().enumerate() {
                let (binding, is_never) = this.shapes.closure_param(ctx, *func_id, index, param)?;
                let ty = binding.carrier;
                let value = arguments.get(index).copied();
                ensure_diag!(
                    value.is_some() || param.default.is_some() || admits_undefined(ctx, ty),
                    codes::TYPE_MISMATCH,
                    "missing required callback argument"
                );
                this.bind_parameter(ctx, param, value, binding, is_never)?;
            }
            this.inline_body(ctx, body, result_type)
        })?;
        match slot {
            Some(slot) => self.load(ctx, slot),
            None => {
                let ty = FP64Type::get(ctx).into();
                Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty)))
            }
        }
    }
}
