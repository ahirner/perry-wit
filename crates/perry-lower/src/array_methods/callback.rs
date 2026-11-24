use super::*;
use crate::inline::InlineResult;

impl<'s> FunctionBuilder<'s> {
    /// Apply an array callback (a named function or an inline closure) to `arguments`. An
    /// inline closure's body lowers in place, with its `return` statements branching to a `done` block.
    pub(crate) fn array_callback(
        &mut self,
        ctx: &mut Context,
        callback: &'s Expr,
        arguments: &[Value],
    ) -> Result<Value> {
        self.inline_callback(ctx, callback, arguments, None)
    }

    /// Apply a callback whose result the caller discards, as `forEach` does.
    pub(crate) fn effect_callback(
        &mut self,
        ctx: &mut Context,
        callback: &'s Expr,
        arguments: &[Value],
    ) -> Result<()> {
        self.inline_callback(ctx, callback, arguments, Some(InlineResult::Void))
            .map(|_| ())
    }

    /// Apply `callback` to `arguments`. An inline closure stores the `expected` result, or else
    /// the result type the frontend inferred for it.
    pub(crate) fn inline_callback(
        &mut self,
        ctx: &mut Context,
        callback: &'s Expr,
        arguments: &[Value],
        expected: Option<InlineResult>,
    ) -> Result<Value> {
        let callback = self.shapes.callable(callback);
        if let Expr::FuncRef(id) = callback {
            let signature = self.signature(*id)?;
            ensure_unsupported!(
                !signature.is_async && signature.params.iter().all(|p| !p.source.is_rest),
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
        let result_type = match (expected, return_type) {
            (Some(result), _) => result,
            (None, HirType::Void) => InlineResult::Void,
            (None, ty) => InlineResult::Declared(self.shapes.value_type(ctx, ty)?),
        };
        let slot = self.with_inline_scope(ctx, self.this_slot, |this, ctx| {
            for (index, param) in params.iter().enumerate() {
                let ty = this.shapes.value_type(ctx, &param.ty)?;
                let value = match arguments.get(index) {
                    Some(&value) => value,
                    None => {
                        ensure_diag!(
                            param.default.is_some() || admits_undefined(ctx, ty),
                            codes::TYPE_MISMATCH,
                            "missing required callback argument"
                        );
                        let ty = crate::coerce::admitting(ctx, ty, perry_mir::Absence::Undefined);
                        this.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
                    }
                };
                this.bind_parameter(ctx, param, Some(value), BindingType::plain(ty))?;
            }
            this.inline_body(ctx, body, params, result_type)
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
