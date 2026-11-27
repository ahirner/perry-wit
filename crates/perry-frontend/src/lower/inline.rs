//! Return frames for inline callbacks and class callables.
use super::*;

/// The result of an inlined call. The frontend types every inline callable's result, so
/// lowering never infers one from the returned values.
#[derive(Clone, Copy)]
pub(super) enum InlineResult {
    Void,
    Declared(TypeHandle),
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
        let done = match result_type {
            InlineResult::Declared(ty) => self.result_block(ctx, ty),
            InlineResult::Void => self.new_block(ctx, "inline_done"),
        };
        self.frame.return_to = Some(done);
        self.invalidate_inline_writes(body, []);
        self.statements(ctx, body)?;
        if self.reachable(ctx, self.block) {
            let value = self.inline_result(ctx, done, None)?;
            self.emit(BrOp::new(ctx, done, value.into_iter().collect()), ctx);
        }
        self.switch_to(done);
        Ok(done.deref(ctx).arguments().next())
    }

    pub(super) fn inline_return(
        &mut self,
        ctx: &mut Context,
        done: Ptr<BasicBlock>,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        let value = self.inline_result(ctx, done, expression)?;
        self.run_finallies(ctx, 0)?;
        if !self.terminated(ctx) {
            self.emit(BrOp::new(ctx, done, value.into_iter().collect()), ctx);
        }
        self.after_terminator(ctx);
        Ok(())
    }

    fn inline_result(
        &mut self,
        ctx: &mut Context,
        done: Ptr<BasicBlock>,
        expression: Option<&'s Expr>,
    ) -> Result<Option<Value>> {
        let Some(result) = done.deref(ctx).arguments().next() else {
            if let Some(expr) = expression {
                self.effect(ctx, expr)?;
            }
            return Ok(None);
        };
        let ty = result.get_type(ctx);
        let value = match expression
            .map(|expr| self.optional_operand(ctx, expr, Some(ty)))
            .transpose()?
            .flatten()
        {
            Some(value) => value,
            None => self.emit_value(UndefinedOp::with_type(ctx, ty), ctx),
        };
        self.fit(ctx, value, ty).map(Some)
    }
}
