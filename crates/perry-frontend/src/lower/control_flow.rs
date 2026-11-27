//! Structured source control flow expressed as ordinary typed MIR blocks.
use super::*;
use crate::lower::collections::LiveCursor;

mod traversal;
pub(super) use traversal::{ArrayOutput, Holes, Input, Span};

#[derive(Default)]
pub(super) struct LoopUpdate<'s> {
    pub(super) expression: Option<&'s Expr>,
    pub(super) cells: Vec<LocalId>,
}

#[derive(Clone, Copy)]
pub(super) enum LoopCondition<'s> {
    Before(Option<&'s Expr>),
    After(&'s Expr),
    Indices(Value, LoopBound),
}

#[derive(Clone, Copy)]
pub(super) enum LoopBound {
    Fixed(Value),
    Array(Value),
    Collection(Value),
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn enter_case(&mut self, ctx: &mut Context, test: Value) -> Ptr<BasicBlock> {
        let matched = self.new_block(ctx, "case");
        let next = self.new_block(ctx, "next");
        self.branch(ctx, test, matched, next);
        self.switch_to(matched);
        next
    }

    pub(super) fn reachable(&self, ctx: &Context, block: Ptr<BasicBlock>) -> bool {
        let entry = self.entry_block();
        let mut seen = HashSet::new();
        let mut pending = vec![block];
        while let Some(block) = pending.pop() {
            if block == entry {
                return true;
            }
            if seen.insert(block) {
                pending.extend(block.preds(ctx));
            }
        }
        false
    }

    pub(super) fn switch(
        &mut self,
        ctx: &mut Context,
        discriminant: &'s Expr,
        cases: &'s [perry_hir::ir::SwitchCase],
        label: Option<&'s str>,
    ) -> Result<()> {
        let value = self.expression(ctx, discriminant)?;
        let ty = value.get_type(ctx);
        ensure_unsupported!(
            matches!(
                kind(ctx, value)?,
                Kind::Number | Kind::String | Kind::Boolean | Kind::BigInt
            ),
            "switch requires a typed scalar discriminant"
        );
        let done = self.new_block(ctx, "switch_done");
        let blocks: Vec<_> = cases
            .iter()
            .map(|_| self.new_block(ctx, "switch_case"))
            .collect();
        let default = cases
            .iter()
            .zip(&blocks)
            .find(|(case, _)| case.test.is_none())
            .map_or(done, |(_, &block)| block);
        ensure_internal!(
            cases.iter().filter(|case| case.test.is_none()).count() <= 1,
            "switch has more than one default"
        );
        for (case, &block) in cases.iter().zip(&blocks) {
            if let Some(test) = &case.test {
                let test = self.expression_with(ctx, test, Some(ty))?;
                let test = self.fit(ctx, test, ty)?;
                let matches = self.compare(ctx, CompareKind::Eq, value, test);
                let next = self.new_block(ctx, "switch_test");
                self.branch(ctx, matches, block, next);
                self.switch_to(next);
            }
        }
        self.jump(ctx, default);
        self.frame.controls.push(Control::Switch {
            label,
            break_to: done,
        });
        let guards = self.variant_guards.depth();
        let mut fallthrough = false;
        for (index, (case, &block)) in cases.iter().zip(&blocks).enumerate() {
            self.variant_guards.truncate(guards);
            if !fallthrough {
                if let Some(test) = &case.test {
                    self.variant_guards.equality(discriminant, test, true);
                } else {
                    for test in cases.iter().filter_map(|case| case.test.as_ref()) {
                        self.variant_guards.equality(discriminant, test, false);
                    }
                }
            }
            self.switch_to(block);
            self.statements(ctx, &case.body)?;
            fallthrough = self.reachable(ctx, self.block);
            self.jump(ctx, blocks.get(index + 1).copied().unwrap_or(done));
        }
        self.variant_guards.truncate(guards);
        self.frame.controls.pop();
        self.switch_to(done);
        Ok(())
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    /// Leave with exception `status`: to the innermost handler, or out of the function. Ends
    /// the current block.
    pub(super) fn propagate(&mut self, ctx: &mut Context, status: Value) {
        match self.handler {
            Some(handler) => {
                self.store(ctx, handler.status, status);
                self.emit(ctx, |ctx| BrOp::new(ctx, handler.block, vec![]));
            }
            None => {
                self.emit(ctx, |ctx| RethrowOp::new(ctx, status));
            }
        }
    }

    /// Run the bodies of the `finally` blocks that a jump out of them leaves, innermost first,
    /// with only the handlers outside each `try` active.
    pub(super) fn run_finallies(&mut self, ctx: &mut Context, depth: usize) -> Result<()> {
        let cleanup = self
            .frame
            .controls
            .iter()
            .enumerate()
            .skip(depth)
            .rev()
            .find_map(|(i, control)| match *control {
                Control::Finally { body, handler } => Some((i, body, handler)),
                _ => None,
            });
        let Some((index, body, handler)) = cleanup else {
            return Ok(());
        };
        let inner = self.frame.controls.split_off(index);
        let result = self.with_handler(ctx, handler, None, |b, ctx| {
            b.statements(ctx, body)?;
            if !b.terminated(ctx) {
                b.run_finallies(ctx, depth)?;
            }
            Ok(())
        });
        self.frame.controls.extend(inner);
        result
    }

    /// `try { body } catch (param) { handler } finally { cleanup }`.
    pub(super) fn try_(
        &mut self,
        ctx: &mut Context,
        body: &'s [Stmt],
        catch: Option<&'s perry_hir::ir::CatchClause>,
        finally: Option<&'s [Stmt]>,
    ) -> Result<()> {
        let i32 = IntegerType::get(ctx, 32, Signedness::Signless).into();
        let status = self.entry_slot(ctx, i32);
        let join = self.new_block(ctx, "join");
        // The exceptional path of `finally`: run it, then throw the exception again.
        let rethrow_block = finally.map(|_| self.new_block(ctx, "finally"));
        let catch_block = catch.map(|_| self.new_block(ctx, "catch"));
        let first = catch_block
            .or(rethrow_block)
            .ok_or_else(|| Diagnostic::internal("try without catch or finally"))?;

        let guards = self.variant_guards.depth();
        self.with_handler(
            ctx,
            Some(Handler {
                block: first,
                status,
            }),
            finally,
            |b, ctx| b.statements(ctx, body),
        )?;
        if !self.terminated(ctx) {
            self.statements(ctx, finally.unwrap_or_default())?;
            self.jump(ctx, join);
        }
        self.variant_guards.truncate(guards);

        if let (Some(clause), Some(block)) = (catch, catch_block) {
            self.switch_to(block);
            let exception = self.catch_exception(ctx, status)?;
            if let Some((id, _)) = &clause.param {
                let binding = self.initialized_local(ctx, exception);
                self.variant_guards.assign(*id);
                self.locals.insert(*id, binding);
            }
            let handler = rethrow_block
                .map(|block| Handler { block, status })
                .or(self.handler);
            self.with_handler(ctx, handler, finally, |b, ctx| {
                b.statements(ctx, &clause.body)
            })?;
            if !self.terminated(ctx) {
                self.statements(ctx, finally.unwrap_or_default())?;
                self.jump(ctx, join);
            }
            self.variant_guards.truncate(guards);
        }

        if let Some(block) = rethrow_block {
            self.switch_to(block);
            let exception = self.catch_exception(ctx, status)?;
            self.statements(ctx, finally.unwrap_or_default())?;
            if !self.terminated(ctx) {
                let again = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, exception));
                self.propagate(ctx, again);
            }
            self.variant_guards.truncate(guards);
        }
        self.switch_to(join);
        Ok(())
    }

    pub(super) fn catch_handler(&mut self, ctx: &mut Context, label: &str) -> Handler {
        let ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
        Handler {
            block: self.new_block(ctx, label),
            status: self.entry_slot(ctx, ty),
        }
    }

    pub(super) fn catch_exception(&mut self, ctx: &mut Context, slot: Value) -> Result<Value> {
        let caught = self.load(ctx, slot)?;
        self.pass_exit(ctx, caught);
        Ok(self.emit_value(ctx, |ctx| CatchOp::new(ctx, caught)))
    }

    /// `process.exit` unwinds through handlers without running them.
    fn pass_exit(&mut self, ctx: &mut Context, status: Value) {
        let exit = self.i32(ctx, EXIT_STATUS);
        let exiting = self.compare(ctx, CompareKind::Eq, status, exit);
        let leave = self.new_block(ctx, "exit");
        let stay = self.new_block(ctx, "handle");
        self.branch(ctx, exiting, leave, stay);
        self.switch_to(leave);
        if self.handler.is_none() {
            self.emit(ctx, |ctx| {
                let rethrow = RethrowOp::new(ctx, status);
                perry_mir::set_exit_only(ctx, &rethrow);
                rethrow
            });
        } else {
            self.propagate(ctx, status);
        }
        self.switch_to(stay);
    }

    /// `for (init; condition; update) body`, `while`, and `do … while`.
    pub(super) fn loop_(
        &mut self,
        ctx: &mut Context,
        init: Option<&'s Stmt>,
        condition: LoopCondition<'s>,
        update: LoopUpdate<'s>,
        body: &'s [Stmt],
        label: Option<&'s str>,
    ) -> Result<()> {
        if let Some(init) = init {
            self.statements(ctx, std::slice::from_ref(init))?;
        }
        let tested = match condition {
            LoopCondition::Before(condition) => condition,
            LoopCondition::After(condition) => Some(condition),
            LoopCondition::Indices(..) => None,
        };
        let guards = self.variant_guards.depth();
        self.invalidate_inline_writes(body, tested.into_iter().chain(update.expression));
        let lowered = self.repeat(
            ctx,
            "source",
            condition,
            update,
            None,
            |b, ctx, next, done| {
                b.frame.controls.push(Control::Loop {
                    label,
                    continue_to: next,
                    break_to: done,
                });
                let lowered = b.statements(ctx, body);
                b.frame.controls.pop();
                lowered
            },
        );
        self.variant_guards.truncate(guards);
        lowered
    }

    pub(super) fn for_of(
        &mut self,
        ctx: &mut Context,
        element: LocalId,
        source: &'s Expr,
        body: &'s [Stmt],
        label: Option<&'s str>,
    ) -> Result<()> {
        if matches!(source, Expr::UrlSearchParamsNew(_)) {
            bail_unsupported!(
                "synchronous for...of requires a dense typed array or string; URLSearchParams iteration is unsupported"
            );
        }
        let value = self.expression(ctx, source)?;
        let value = self.defined(ctx, value)?;
        let input = match kind(ctx, value)? {
            Kind::Array => Input::LiveArray(value),
            Kind::String => Input::String(value),
            Kind::Record
                if crate::lower::collections::Collection::of(ctx, value.get_type(ctx))
                    .is_some() =>
            {
                Input::Collection(value)
            }
            _ => bail_unsupported!(
                "synchronous for...of requires a dense typed array or string; this receiver has no supported iterator"
            ),
        };
        let guards = self.variant_guards.depth();
        self.invalidate_inline_writes(body, []);
        let result = self.iterate(ctx, input, |b, ctx, item, _, next, done| {
            let local = b.initialized_local(ctx, item);
            b.locals.insert(element, local);
            b.frame.controls.push(Control::Loop {
                label,
                continue_to: next,
                break_to: done,
            });
            let result = b.statements(ctx, body);
            b.frame.controls.pop();
            result
        });
        self.variant_guards.truncate(guards);
        result
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    fn repeat<T>(
        &mut self,
        ctx: &mut Context,
        label: &str,
        condition: LoopCondition<'s>,
        update: LoopUpdate<'s>,
        cursor: Option<(Value, LiveCursor)>,
        body: impl FnOnce(&mut Self, &mut Context, Ptr<BasicBlock>, Ptr<BasicBlock>) -> Result<T>,
    ) -> Result<T> {
        let header = self.new_block(ctx, &format!("{label}_header"));
        let entered = self.new_block(ctx, &format!("{label}_body"));
        let next = self.new_block(ctx, &format!("{label}_next"));
        let done = self.new_block(ctx, &format!("{label}_done"));
        let exhausted = if cursor.is_some() {
            self.new_block(ctx, "loop_exhausted")
        } else {
            done
        };
        let body_first = matches!(condition, LoopCondition::After(_));
        self.jump(ctx, if body_first { entered } else { header });
        self.switch_to(header);
        let test = match condition {
            LoopCondition::Before(None | Some(Expr::Bool(true))) => None,
            LoopCondition::Before(Some(condition)) | LoopCondition::After(condition) => {
                Some(self.condition(ctx, condition)?)
            }
            LoopCondition::Indices(index, bound) => {
                let current = self.load(ctx, index)?;
                let limit = match bound {
                    LoopBound::Fixed(limit) => limit,
                    LoopBound::Array(array) => self.array_length(ctx, array),
                    LoopBound::Collection(collection) => {
                        self.collection_backing_length(ctx, collection)?
                    }
                };
                Some(self.compare(ctx, CompareKind::Lt, current, limit))
            }
        };
        match test {
            Some(test) => self.branch(ctx, test, entered, exhausted),
            None => self.jump(ctx, entered),
        }
        self.switch_to(entered);
        let output = body(self, ctx, next, done)?;
        self.jump(ctx, next);
        self.switch_to(next);
        for id in update.cells {
            let slot = self.local(id)?;
            let old = self.load(ctx, slot)?;
            let record = perry_mir::typed::<RecordType>(ctx, old.get_type(ctx))
                .ok_or_else(|| Diagnostic::internal("loop cell has no record layout"))?;
            let value = self.record_field(ctx, old, "v")?;
            let fresh = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, vec![value]));
            self.store(ctx, slot, fresh);
        }
        if let Some(update) = update.expression {
            self.effect(ctx, update)?;
        }
        if let Some((index, cursor)) = cursor {
            let position = cursor.position(self, ctx)?;
            self.store(ctx, index, position);
        } else if let LoopCondition::Indices(index, _) = condition {
            let current = self.load(ctx, index)?;
            let one = self.number(ctx, 1.0);
            let advanced = self.binary(ctx, BinaryKind::Add, current, one);
            self.store(ctx, index, advanced);
        }
        self.jump(ctx, header);
        if let Some((_, cursor)) = cursor {
            self.switch_to(exhausted);
            cursor.exhausted(self, ctx);
            self.jump(ctx, done);
        }
        self.switch_to(done);
        Ok(output)
    }

    pub(crate) fn index_loop<T>(
        &mut self,
        ctx: &mut Context,
        label: &str,
        start: Value,
        limit: Value,
        body: impl FnOnce(&mut Self, &mut Context, Value, Ptr<BasicBlock>, Ptr<BasicBlock>) -> Result<T>,
    ) -> Result<T> {
        let index = self.entry_slot(ctx, start.get_type(ctx));
        self.store(ctx, index, start);
        self.repeat(
            ctx,
            label,
            LoopCondition::Indices(index, LoopBound::Fixed(limit)),
            LoopUpdate::default(),
            None,
            |b, ctx, next, done| {
                let current = b.load(ctx, index)?;
                body(b, ctx, current, next, done)
            },
        )
    }
}
