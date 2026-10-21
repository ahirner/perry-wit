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

    pub(crate) fn inline_callback(
        &mut self,
        ctx: &mut Context,
        callback: &'s Expr,
        arguments: &[Value],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let callback = match callback {
            Expr::LocalGet(id) => self.closures.get(id).copied().unwrap_or(callback),
            _ => callback,
        };
        if let Expr::FuncRef(id) = callback {
            let signature = self.signatures.get(id).context("unknown callback")?;
            ensure!(
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
            let call = CallOp::new(
                ctx,
                signature.name.clone(),
                values,
                signature.carrier_results(),
            );
            self.append(ctx, &call);
            return Ok(if signature.result.is_none() {
                let ty = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
            } else {
                call.get_operation().deref(ctx).get_result(0)
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
            bail!("array callbacks require an inline closure or named function")
        };
        ensure!(
            !is_async && !is_generator && params.iter().all(|p| !p.is_rest),
            "array callbacks must be synchronous; rest parameters are not supported"
        );
        let declared = match return_type {
            HirType::Number | HirType::String | HirType::Boolean | HirType::Union(_) => {
                self.shapes.value_type(ctx, return_type).ok()
            }
            _ => None,
        };
        let result_type = expected
            .or(declared)
            .map_or(InlineResult::Inferred, InlineResult::Declared);
        let slot = self.with_inline_scope(ctx, self.this_slot, |this, ctx| {
            for (index, param) in params.iter().enumerate() {
                let value = match arguments.get(index) {
                    Some(&value) => value,
                    None => {
                        let ty = this.shapes.value_type(ctx, &param.ty)?;
                        ensure!(
                            param.default.is_some() || admits_undefined(ctx, ty),
                            "missing required callback argument"
                        );
                        let ty = crate::coerce::admitting(ctx, ty, perry_mir::Absence::Undefined);
                        this.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
                    }
                };
                this.bind_parameter(ctx, param, Some(value), None)?;
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
