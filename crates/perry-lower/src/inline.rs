//! Return frames for inline callbacks and class callables.
use super::*;
use perry_mir::OptionalType;

/// The result an inline body stores. The frontend types every inline callable's result, so
/// lowering never infers one from the returned values.
#[derive(Clone, Copy)]
pub(super) enum InlineResult {
    Void,
    Declared(TypeHandle),
}

pub(super) struct ReturnFrame {
    done: Ptr<BasicBlock>,
    /// The result slot; `None` for a void body, whose returned values are evaluated for effects.
    slot: Option<Value>,
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn with_inline_scope<T>(
        &mut self,
        ctx: &mut Context,
        this_slot: Option<Value>,
        lower: impl FnOnce(&mut Self, &mut Context) -> Result<T>,
    ) -> Result<T> {
        let locals = self.locals.scope();
        let saved_this = std::mem::replace(&mut self.this_slot, this_slot);
        let guards = self.variant_guards.depth();
        let finallies = std::mem::take(&mut self.finallies);
        let controls = std::mem::take(&mut self.controls);
        let result = lower(self, ctx);
        self.locals.restore(locals);
        self.this_slot = saved_this;
        self.variant_guards.truncate(guards);
        self.finallies = finallies;
        self.controls = controls;
        result
    }

    pub(super) fn invalidate_inline_writes(
        &mut self,
        body: &'s [Stmt],
        exprs: impl IntoIterator<Item = &'s Expr>,
    ) {
        self.variant_guards.assign_in(body, exprs, |id| {
            if let Some(Expr::Closure { body, .. }) = self.shapes.lexical_functions.get(&id) {
                return Some(body.as_slice());
            }
            self.signatures.get(&id)?.inline.map(|f| f.body.as_slice())
        });
    }

    pub(super) fn inline_body(
        &mut self,
        ctx: &mut Context,
        body: &'s [Stmt],
        params: &'s [perry_frontend::hir::ir::Param],
        result_type: InlineResult,
    ) -> Result<Option<Value>> {
        let done = self.new_block(ctx, "inline_done");
        let slot = match result_type {
            InlineResult::Declared(ty) => {
                let ty = if returns_optional(body) {
                    OptionalType::get(ctx, ty).into()
                } else {
                    ty
                };
                Some(self.entry_slot(ctx, ty))
            }
            InlineResult::Void => None,
        };
        self.inline_returns.push(ReturnFrame { done, slot });
        let body = hir_compat::strip_synthetic_default_guards(body, params.iter());
        // An inline callback runs once per element.
        self.invalidate_inline_writes(body, []);
        let mut result = self.statements(ctx, body);
        if result.is_ok() && self.reachable(ctx, self.block) {
            if matches!(result_type, InlineResult::Void) {
                self.jump(ctx, done);
            } else {
                result = self.inline_return(ctx, None);
            }
        }
        self.inline_returns
            .pop()
            .ok_or_else(|| Diagnostic::internal("inline return frame"))?;
        result?;
        self.switch_to(done);
        Ok(slot)
    }

    /// `return` inside the innermost inline function: store the result and leave for its `done`
    /// block through the function's own `finally` bodies.
    pub(super) fn inline_return(
        &mut self,
        ctx: &mut Context,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        let Some(frame) = self.inline_returns.pop() else {
            bail_internal!("return outside an inline function")
        };
        // The frame is off the stack only while the result is stored: a `finally` body run
        // below may itself `return` from this frame.
        let stored = self.store_inline_result(ctx, &frame, expression);
        let done = frame.done;
        self.inline_returns.push(frame);
        stored?;
        self.run_finallies(ctx, |_| true)?;
        self.jump(ctx, done);
        self.after_terminator(ctx);
        Ok(())
    }

    /// Store a returned value in the body's result slot. A void body evaluates it for effects.
    fn store_inline_result(
        &mut self,
        ctx: &mut Context,
        frame: &ReturnFrame,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        let Some(slot) = frame.slot else {
            if let Some(expr) = expression {
                self.effect(ctx, expr)?;
            }
            return Ok(());
        };
        let ty = self.slot_type(ctx, slot)?;
        let value = match expression
            .map(|expr| self.optional_operand(ctx, expr, Some(ty)))
            .transpose()?
            .flatten()
        {
            Some(value) => value,
            None => self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)),
        };
        let value = self.fit(ctx, value, ty)?;
        self.store(ctx, slot, value);
        Ok(())
    }
}

// How a statement list may complete, as a bit set.
pub(super) use perry_frontend::completion::returns_optional;
