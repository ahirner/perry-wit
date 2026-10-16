//! Structured source control flow expressed as ordinary typed MIR blocks.
use super::*;
use crate::collections::LiveCursor;

impl<'s> FunctionBuilder<'s> {
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
    ) -> Result<()> {
        let value = self.expression(ctx, discriminant)?;
        let ty = value.get_type(ctx);
        ensure!(
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
        ensure!(
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
        self.controls.push(Control::Switch { break_to: done });
        let guards = self.variant_guards.len();
        let mut fallthrough = false;
        for (index, (case, &block)) in cases.iter().zip(&blocks).enumerate() {
            self.variant_guards.truncate(guards);
            if !fallthrough {
                if let Some(test) = &case.test {
                    self.variant_equality(discriminant, test, true);
                } else {
                    for test in cases.iter().filter_map(|case| case.test.as_ref()) {
                        self.variant_equality(discriminant, test, false);
                    }
                }
            }
            self.switch_to(block);
            self.statements(ctx, &case.body)?;
            fallthrough = self.reachable(ctx, self.block);
            self.jump(ctx, blocks.get(index + 1).copied().unwrap_or(done));
        }
        self.variant_guards.truncate(guards);
        self.controls.pop();
        self.switch_to(done);
        Ok(())
    }
}

impl<'s> FunctionBuilder<'s> {
    /// Leave with exception `status`: to the innermost handler, or out of the function. Ends
    /// the current block.
    pub(super) fn propagate(&mut self, ctx: &mut Context, status: Value) {
        match self.handlers.last().copied() {
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
    pub(super) fn run_finallies(
        &mut self,
        ctx: &mut Context,
        leaving: impl Fn(&Finally) -> bool,
    ) -> Result<()> {
        let pending: Vec<(usize, Finally<'s>)> = self
            .finallies
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, f)| leaving(f))
            .collect();
        for (index, finally) in pending.into_iter().rev() {
            // Lower the body with this `finally` and everything inside it inactive.
            let handlers = self.handlers.split_off(finally.handlers);
            let finallies = self.finallies.split_off(index);
            let lowered = self.statements(ctx, finally.body);
            self.handlers.extend(handlers);
            self.finallies.extend(finallies);
            lowered?;
            if self.terminated(ctx) {
                break;
            }
        }
        Ok(())
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
        let outer = Finally {
            body: finally.unwrap_or_default(),
            handlers: self.handlers.len(),
            controls: self.controls.len(),
        };
        // The exceptional path of `finally`: run it, then throw the exception again.
        let rethrow_block = finally.map(|_| self.new_block(ctx, "finally"));
        let catch_block = catch.map(|_| self.new_block(ctx, "catch"));
        let first = catch_block
            .or(rethrow_block)
            .context("try without catch or finally")?;

        if finally.is_some() {
            self.finallies.push(outer);
        }
        self.handlers.push(Handler {
            block: first,
            status,
        });
        let lowered = self.statements(ctx, body);
        self.handlers.pop();
        if finally.is_some() {
            self.finallies.pop();
        }
        lowered?;
        if !self.terminated(ctx) {
            self.statements(ctx, outer.body)?;
            self.jump(ctx, join);
        }

        if let (Some(clause), Some(block)) = (catch, catch_block) {
            self.switch_to(block);
            let caught = self.load(ctx, status)?;
            self.pass_exit(ctx, caught);
            let exception = self.emit_value(ctx, |ctx| CatchOp::new(ctx, caught));
            if let Some((id, _)) = &clause.param {
                let ty = exception.get_type(ctx);
                let slot = self.entry_slot(ctx, ty);
                self.store(ctx, slot, exception);
                self.locals.insert(*id, LocalBinding::plain(slot));
            }
            if let Some(rethrow) = rethrow_block {
                self.finallies.push(outer);
                self.handlers.push(Handler {
                    block: rethrow,
                    status,
                });
            }
            let lowered = self.statements(ctx, &clause.body);
            if rethrow_block.is_some() {
                self.handlers.pop();
                self.finallies.pop();
            }
            lowered?;
            if !self.terminated(ctx) {
                self.statements(ctx, outer.body)?;
                self.jump(ctx, join);
            }
        }

        if let Some(block) = rethrow_block {
            self.switch_to(block);
            let caught = self.load(ctx, status)?;
            self.pass_exit(ctx, caught);
            // Hold the exception across the cleanup, which may throw and catch its own.
            let exception = self.emit_value(ctx, |ctx| CatchOp::new(ctx, caught));
            self.statements(ctx, outer.body)?;
            if !self.terminated(ctx) {
                let again = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, exception));
                self.propagate(ctx, again);
            }
        }
        self.switch_to(join);
        Ok(())
    }

    /// `process.exit` unwinds through handlers without running them.
    fn pass_exit(&mut self, ctx: &mut Context, status: Value) {
        let exit = self.i32(ctx, EXIT_STATUS);
        let exiting = self.compare(ctx, CompareKind::Eq, status, exit);
        let leave = self.new_block(ctx, "exit");
        let stay = self.new_block(ctx, "handle");
        self.branch(ctx, exiting, leave, stay);
        self.switch_to(leave);
        if self.handlers.is_empty() {
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
        condition: Option<&'s Expr>,
        update: Option<&'s Expr>,
        body: &'s [Stmt],
        body_first: bool,
    ) -> Result<()> {
        if let Some(init) = init {
            self.statement(ctx, init)?;
        }
        let header = self.new_block(ctx, "header");
        let body_block = self.new_block(ctx, "body");
        let latch = self.new_block(ctx, "latch");
        let exhausted_exit = self.new_block(ctx, "exhausted");
        let exit = self.new_block(ctx, "exit");
        let collection_loop = if body_first {
            None
        } else {
            self.collection_loop(ctx, condition)?
        };
        self.jump(ctx, if body_first { body_block } else { header });

        self.switch_to(header);
        match condition {
            None | Some(Expr::Bool(true)) => self.jump(ctx, body_block),
            Some(Expr::Bool(false)) => self.jump(ctx, exhausted_exit),
            Some(condition) => {
                let condition = self.condition(ctx, condition)?;
                self.branch(ctx, condition, body_block, exhausted_exit);
            }
        }

        self.switch_to(body_block);
        if let Some(CollectionLoop {
            index,
            collection,
            cursor,
        }) = collection_loop
        {
            let idx_val = self.load(ctx, index)?;
            if let Some(cursor) = cursor {
                let one = self.number(ctx, 1.0);
                let next_idx = self.binary(ctx, BinaryKind::Add, idx_val, one);
                cursor.advance(self, ctx, next_idx);
            }
            let entries = self.collection_entries_array(ctx, collection)?;
            let entry = self.element_at(ctx, entries, idx_val)?;
            if let Some(live) = self.collection_entry_live(ctx, entry)? {
                let live_body = self.new_block(ctx, "loop_live_body");
                self.branch(ctx, live, live_body, latch);
                self.switch_to(live_body);
            }
        }
        self.controls.push(Control::Loop {
            continue_to: latch,
            break_to: exit,
        });
        let lowered = self.statements(ctx, body);
        self.controls.pop();
        lowered?;
        self.jump(ctx, latch);

        self.switch_to(latch);
        if let Some(update) = update {
            self.effect(ctx, update)?;
        }
        let cursor = collection_loop.and_then(|lp| lp.cursor.map(|cursor| (lp.index, cursor)));
        if let Some((index, cursor)) = cursor {
            let position = cursor.position(self, ctx)?;
            self.store(ctx, index, position);
        }
        self.jump(ctx, header);

        self.switch_to(exhausted_exit);
        if let Some((_, cursor)) = cursor {
            cursor.exhausted(self, ctx)?;
        }
        self.jump(ctx, exit);

        self.switch_to(exit);
        if let Some((_, cursor)) = cursor {
            cursor.exited(self, ctx)?;
        }
        Ok(())
    }

    /// The live collection a lowered `for...of` (`for (; i < items.length; …)`) walks, with
    /// a cursor so deletes in the body neither skip nor repeat entries.
    fn collection_loop(
        &mut self,
        ctx: &mut Context,
        condition: Option<&'s Expr>,
    ) -> Result<Option<CollectionLoop>> {
        let Some((idx_id, arr_id)) = self.iteration.for_of_loop(condition) else {
            return Ok(None);
        };
        let (Ok(arr_slot), Ok(index)) = (self.local(arr_id), self.local(idx_id)) else {
            return Ok(None);
        };
        let arr_val = self.load(ctx, arr_slot)?;
        let Ok(collection) = self.defined(ctx, arr_val) else {
            return Ok(None);
        };
        let ty = collection.get_type(ctx);
        if !(crate::collections::is_collection_or_view_record(ctx, ty)
            || crate::collections::map_entry_types(ctx, ty).is_some()
            || crate::collections::set_entry_type(ctx, ty).is_some())
        {
            return Ok(None);
        }
        let cursor = LiveCursor::open(self, ctx, collection, index)?;
        Ok(Some(CollectionLoop {
            index,
            collection,
            cursor,
        }))
    }
}

/// A loop over a live collection: the loop's index slot, the collection and its cursor
/// (`None` for bare entry arrays, which have no cursor array).
#[derive(Clone, Copy)]
struct CollectionLoop {
    index: Value,
    collection: Value,
    cursor: Option<LiveCursor>,
}
