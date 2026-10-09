//! Synchronous builtin callbacks use direct calls or inline bodies over captured local slots.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct InlineReturn {
    pub result: Value,
    pub join: Ptr<BasicBlock>,
    pub cleanup_depth: usize,
}

impl FunctionBuilder<'_> {
    pub(super) fn invoke(
        &mut self,
        ctx: &mut Context,
        id: FuncId,
        values: Vec<Value>,
    ) -> Result<Option<Value>> {
        let signature = &self.signatures[&id];
        let name = signature.name.clone();
        let results = signature.results.clone();
        if signature.is_async {
            let promise =
                self.emit_value(ctx, |ctx| AsyncCallOp::new(ctx, name, values, results[0]));
            return Ok(Some(promise));
        }
        let op = CallOp::new(ctx, name, values, results);
        self.append(ctx, &op);
        let operation = op.get_operation().deref(ctx);
        Ok((operation.get_num_results() == 1).then(|| operation.get_result(0)))
    }

    pub(super) fn callback(
        &mut self,
        ctx: &mut Context,
        callback: &Expr,
        arguments: &[Value],
        result_type: TypeHandle,
    ) -> Result<Value> {
        if let Expr::FuncRef(id) = callback {
            let signature = self
                .signatures
                .get(id)
                .context("unknown callback function")?;
            ensure!(
                !signature.is_async,
                "this builtin requires a synchronous callback"
            );
            let params = signature.params.clone();
            ensure!(
                params.len() <= arguments.len(),
                "callback requests unsupported arguments"
            );
            let arguments = arguments
                .iter()
                .zip(params)
                .map(|(&value, ty)| self.fit(ctx, value, ty))
                .collect::<Result<Vec<_>>>()?;
            return match self.invoke(ctx, *id, arguments)? {
                Some(value) => self.fit(ctx, value, result_type),
                None => {
                    let inner = optional_value(ctx, result_type)
                        .context("void callback requires an optional result")?;
                    Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, inner)))
                }
            };
        }
        let Expr::Closure {
            params,
            body,
            is_arrow,
            is_async,
            is_generator,
            captures_this,
            captures_new_target,
            ..
        } = callback
        else {
            bail!(
                "callbacks must be direct function references or non-escaping arrows (subset rule R5)"
            );
        };
        ensure!(
            *is_arrow && !captures_this && !captures_new_target,
            "only arrows without this or new.target are supported here (subset rule R5)"
        );
        ensure!(
            !is_async && !is_generator,
            "this builtin requires a synchronous callback"
        );
        ensure!(
            params.len() <= arguments.len(),
            "callback requests unsupported arguments"
        );
        ensure!(
            params.iter().all(|p| p.default.is_none() && !p.is_rest),
            "callback default and rest parameters are not supported yet"
        );
        // HIR resolves lexical references by LocalId. Its capture list excludes module
        // bindings, which command preparation has moved into this function's local scope.
        let mut locals = self.locals.clone();
        for (param, &argument) in params.iter().zip(arguments) {
            let ty = match param.ty {
                HirType::Any | HirType::Unknown => argument.get_type(ctx),
                _ => self.shapes.value_type(ctx, &param.ty)?,
            };
            let value = self.fit(ctx, argument, ty)?;
            let slot = self.entry_slot(ctx, ty);
            self.store(ctx, slot, value);
            locals.insert(param.id, slot);
        }
        let result = self.entry_slot(ctx, result_type);
        let join = self.new_block(ctx, "callback_return");
        let saved_locals = std::mem::replace(&mut self.locals, locals);
        let saved_results = std::mem::replace(&mut self.results, vec![result_type]);
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_async = std::mem::replace(&mut self.is_async, false);
        let saved_return = self.inline_return.replace(InlineReturn {
            result,
            join,
            cleanup_depth: self.cleanups.len(),
        });
        let lowered = self.statements(ctx, body);
        if lowered.is_ok() && !self.terminated(ctx) {
            let values = self.implicit_results(ctx);
            self.store(ctx, result, values[0]);
            self.jump(ctx, join);
        }
        self.locals = saved_locals;
        self.results = saved_results;
        self.loops = saved_loops;
        self.is_async = saved_async;
        self.inline_return = saved_return;
        lowered?;
        self.switch_to(join);
        self.load(ctx, result)
    }
}
