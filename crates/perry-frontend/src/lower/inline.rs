//! Return frames for inline callbacks and class callables.
use super::*;

/// The result an inline body stores. The frontend types every inline callable's result, so
/// lowering never infers one from the returned values.
#[derive(Clone, Copy)]
pub(super) enum InlineResult {
    Void,
    Declared(TypeHandle),
}

#[derive(Clone, Copy)]
pub(super) struct ReturnFrame {
    done: Ptr<BasicBlock>,
    /// The result slot; `None` for a void body, whose returned values are evaluated for effects.
    slot: Option<Value>,
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn invalidate_inline_writes(
        &mut self,
        body: &'s [Stmt],
        exprs: impl IntoIterator<Item = &'s Expr>,
    ) {
        if self.variant_guards.depth() != 0 {
            let writes = &self.shapes.specialization.writes;
            for region in crate::narrowing::Region::of(body, exprs) {
                if let Some(locals) = writes.get(&(self.frame.owner, region)) {
                    for &local in locals {
                        self.variant_guards.assign(local);
                    }
                }
            }
        }
    }

    pub(super) fn inline_body(
        &mut self,
        ctx: &mut Context,
        body: &'s [Stmt],
        result_type: InlineResult,
    ) -> Result<Option<Value>> {
        let done = self.new_block(ctx, "inline_done");
        let slot = match result_type {
            InlineResult::Declared(ty) => Some(self.entry_slot(ctx, ty)),
            InlineResult::Void => None,
        };
        let frame = ReturnFrame { done, slot };
        self.frame.return_to = Some(frame);
        self.invalidate_inline_writes(body, []);
        self.statements(ctx, body)?;
        if self.reachable(ctx, self.block) {
            self.store_inline_result(ctx, &frame, None)?;
            self.jump(ctx, done);
        }
        self.switch_to(done);
        Ok(slot)
    }

    /// `return` inside the innermost inline function: store the result and leave for its `done`
    /// block through the function's own `finally` bodies.
    pub(super) fn inline_return(
        &mut self,
        ctx: &mut Context,
        frame: ReturnFrame,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        self.store_inline_result(ctx, &frame, expression)?;
        self.run_finallies(ctx, 0)?;
        self.jump(ctx, frame.done);
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
