use super::*;
use crate::lower::callable::Callback;
use crate::lower::inline::InlineResult;

impl<'s> FunctionBuilder<'s, '_> {
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
        let (func_id, source, body, return_type, is_async, is_generator, signature) = match callback
        {
            Callback::Bound {
                call,
                params,
                callee,
            } => {
                let mut inputs = Vec::new();
                for (index, &ty) in params.iter().enumerate() {
                    if !self.shapes.passes_argument(ty) {
                        continue;
                    }
                    let ty = self.shapes.resolved_binding(ty)?.carrier;
                    let value = match arguments.get(index) {
                        Some(&value) => self.fit(ctx, value, ty)?,
                        None => self.absent_literal(ctx, false, Some(ty))?,
                    };
                    inputs.push(value);
                }
                let result = self.invoke_callback(ctx, call, Some(callee), None, &inputs)?;
                return match (result, expected) {
                    (Some(value), Some(InlineResult::Declared(ty))) => self.fit(ctx, value, ty),
                    (Some(value), _) => Ok(value),
                    (None, _) => self.absent_literal(ctx, false, None),
                };
            }
            Callback::Function(id) => {
                let signature = self.signature(id)?;
                ensure_unsupported!(
                    !signature.is_async && signature.params.iter().all(|p| !p.is_rest),
                    "array callbacks require synchronous functions without rest parameters"
                );
                return Ok(
                    match self.call_values(ctx, id, arguments.iter().copied())? {
                        Some(value) => value,
                        None => {
                            let ty = FP64Type::get(ctx).into();
                            self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
                        }
                    },
                );
            }
            Callback::Lexical(function, signature) => (
                function.id,
                &[][..],
                function.body,
                function.return_type,
                function.is_async,
                function.is_generator,
                Some(signature),
            ),
            Callback::Source(Expr::Closure {
                func_id,
                params,
                body,
                return_type,
                is_async,
                is_generator,
                ..
            }) => (
                *func_id,
                params.as_slice(),
                crate::source::authored_body(params, body),
                return_type,
                *is_async,
                *is_generator,
                None,
            ),
            Callback::Source(_) => {
                bail_unsupported!("array callbacks require an inline closure or named function")
            }
        };
        let params = source.iter().map(|p| (p, None)).chain(
            signature
                .into_iter()
                .flat_map(|s| &s.params)
                .filter_map(|p| match p.origin {
                    crate::lower::program::Parameter::Source(_, param) => {
                        Some((param, Some((p.base, p.is_never))))
                    }
                    _ => None,
                }),
        );
        ensure_unsupported!(
            !is_async && !is_generator && params.clone().all(|(p, _)| !p.is_rest),
            "array callbacks must be synchronous; rest parameters are not supported"
        );
        let result_type = match expected {
            Some(result) => result,
            None if let Some(signature) = signature => {
                signature.result.map_or(InlineResult::Void, |result| {
                    InlineResult::Declared(result.carrier)
                })
            }
            None => self
                .shapes
                .source_closure_result(return_type)?
                .map_or(InlineResult::Void, InlineResult::Declared),
        };
        let slot = self.with_inline_scope(ctx, func_id, self.this_slot, |this, ctx| {
            for (index, (param, projected)) in params.enumerate() {
                let (binding, is_never) = match projected {
                    Some(binding) => binding,
                    None => this.shapes.param_type(None, &param.ty)?,
                };
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
