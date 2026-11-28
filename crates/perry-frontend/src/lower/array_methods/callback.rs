use super::*;
use crate::lower::callable::Callback;
use crate::lower::inline::InlineResult;

impl<'s> FunctionBuilder<'s, '_> {
    pub(in crate::lower) fn callback_ordinal(&mut self, ctx: &mut Context) -> Value {
        let zero = self.number(ctx, 0.0);
        let index = self.entry_slot(ctx, zero.get_type(ctx));
        self.store(ctx, index, zero);
        index
    }

    pub(in crate::lower) fn indexed_callback(
        &mut self,
        ctx: &mut Context,
        callback: Callback<'s>,
        index: Value,
        item: Value,
    ) -> Result<Value> {
        let ordinal = self.load(ctx, index)?;
        let result = self.array_callback(
            ctx,
            callback,
            &crate::collections::Yield::Pair(ordinal, item).indexed_args(),
        )?;
        let one = self.number(ctx, 1.0);
        let next = self.binary(ctx, BinaryKind::Add, ordinal, one);
        self.store(ctx, index, next);
        Ok(result)
    }

    /// Apply an array callback (a named function or an inline closure) to `arguments`. An
    /// inline closure's body lowers in place, with its `return` statements branching to a `done` block.
    pub(in crate::lower) fn array_callback(
        &mut self,
        ctx: &mut Context,
        callback: Callback<'s>,
        arguments: &[Value],
    ) -> Result<Value> {
        match self.invoke_callback(ctx, callback, arguments, None)? {
            Some(value) => Ok(value),
            None => self.absent_literal(ctx, false, None),
        }
    }

    /// Apply `callback` to `arguments`. An inline closure stores the `expected` result, or else
    /// the result type the frontend inferred for it.
    pub(in crate::lower) fn invoke_callback(
        &mut self,
        ctx: &mut Context,
        callback: Callback<'s>,
        arguments: &[Value],
        expected: Option<InlineResult>,
    ) -> Result<Option<Value>> {
        let value = match callback {
            Callback::Function(signature, prefix) => {
                ensure_unsupported!(
                    prefix.is_some() || !signature.is_async && signature.parameters().1.is_none(),
                    "array callbacks require synchronous functions without rest parameters"
                );
                let supplied = signature.supplied_values(usize::from(prefix.is_some()), arguments);
                self.invoke_arguments(
                    ctx,
                    signature,
                    prefix.into_iter().chain(supplied),
                    [],
                    expected,
                )?
            }
            Callback::Source(decl) => {
                ensure_unsupported!(
                    !decl.is_async && !decl.is_generator && decl.params.iter().all(|p| !p.is_rest),
                    "array callbacks require synchronous functions without rest parameters"
                );
                let result = match expected {
                    Some(result) => result,
                    None => {
                        if decl.result == crate::engine::types::Ty::VOID {
                            InlineResult::Void
                        } else {
                            InlineResult::Declared(
                                self.shapes.resolved_binding(decl.result)?.carrier,
                            )
                        }
                    }
                };
                self.with_inline_scope(ctx, decl.func_id, self.frame.this_slot, |b, ctx| {
                    for (index, param) in decl.params.iter().enumerate() {
                        let ty = b
                            .shapes
                            .specialization
                            .instances
                            .local(param.id)?
                            .annotation;
                        let (binding, is_never) = b
                            .shapes
                            .native_param(crate::engine::NativeParam::Value(ty))?;
                        let value = arguments.get(index).copied();
                        ensure_diag!(
                            value.is_some()
                                || param.default.is_some()
                                || admits_undefined(ctx, binding.carrier),
                            codes::TYPE_MISMATCH,
                            "missing required callback argument"
                        );
                        b.bind_parameter(ctx, param, value, binding, is_never)?;
                    }
                    b.inline_body(ctx, decl.body, result)
                })?
            }
        };
        match (value, expected) {
            (Some(value), Some(InlineResult::Declared(ty))) => self.fit(ctx, value, ty).map(Some),
            (value, _) => Ok(value),
        }
    }
}
