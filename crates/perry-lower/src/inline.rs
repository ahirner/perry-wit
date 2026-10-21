//! Return frames for inline callbacks and class callables.
use super::*;
use crate::builder::insert_before_terminator;
use perry_mir::OptionalType;

pub(super) enum InlineResult {
    Void,
    Inferred,
    Declared(TypeHandle),
}

pub(super) struct ReturnFrame {
    done: Ptr<BasicBlock>,
    slot: Option<Value>,
    optional: bool,
    // Empty returns preceding the first typed return need stores once its slot exists.
    empty_returns: Vec<Ptr<BasicBlock>>,
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn with_inline_scope<T>(
        &mut self,
        ctx: &mut Context,
        this_slot: Option<Value>,
        lower: impl FnOnce(&mut Self, &mut Context) -> Result<T>,
    ) -> Result<T> {
        let locals = self.locals.scope();
        let closures = self.closures.scope();
        let saved_this = std::mem::replace(&mut self.this_slot, this_slot);
        let guards = self.variant_guards.len();
        let finallies = std::mem::take(&mut self.finallies);
        let controls = std::mem::take(&mut self.controls);
        let result = lower(self, ctx);
        self.locals.restore(locals);
        self.closures.restore(closures);
        self.this_slot = saved_this;
        self.variant_guards.truncate(guards);
        self.finallies = finallies;
        self.controls = controls;
        result
    }

    pub(super) fn inline_body(
        &mut self,
        ctx: &mut Context,
        body: &'s [Stmt],
        params: &'s [perry_hir::ir::Param],
        result_type: InlineResult,
    ) -> Result<Option<Value>> {
        let done = self.new_block(ctx, "inline_done");
        let optional = !matches!(result_type, InlineResult::Void) && returns_optional(body);
        let slot = match result_type {
            InlineResult::Declared(ty) => {
                let ty = if optional {
                    OptionalType::get(ctx, ty).into()
                } else {
                    ty
                };
                Some(self.entry_slot(ctx, ty))
            }
            _ => None,
        };
        self.inline_returns.push(ReturnFrame {
            done,
            slot,
            optional,
            empty_returns: Vec::new(),
        });
        let body = hir_compat::strip_synthetic_default_guards(body, params.iter());
        let mut result = self.statements(ctx, body);
        if result.is_ok() && self.reachable(ctx, self.block) {
            if matches!(result_type, InlineResult::Void) {
                self.jump(ctx, done);
            } else {
                result = self.inline_return(ctx, None);
            }
        }
        let frame = self.inline_returns.pop().context("inline return frame")?;
        result?;
        let slot = match frame.slot {
            Some(slot) => Some(slot),
            None if matches!(result_type, InlineResult::Void) || frame.empty_returns.is_empty() => {
                None
            }
            None => {
                let number = FP64Type::get(ctx).into();
                let ty = OptionalType::get(ctx, number).into();
                Some(self.entry_slot(ctx, ty))
            }
        };
        if let Some(slot) = slot
            && !frame.empty_returns.is_empty()
        {
            let ty = self.slot_type(ctx, slot)?;
            for block in frame.empty_returns {
                let undefined = UndefinedOp::with_type(ctx, ty);
                let store = StoreOp::new(ctx, slot, undefined.get_result(ctx));
                insert_before_terminator(
                    ctx,
                    block,
                    [undefined.get_operation(), store.get_operation()],
                )?;
            }
        }
        self.switch_to(frame.done);
        Ok(slot)
    }

    /// `return` inside the innermost inline function: store the result and leave for its `done`
    /// block through the function's own `finally` bodies.
    pub(super) fn inline_return(
        &mut self,
        ctx: &mut Context,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        let Some(mut frame) = self.inline_returns.pop() else {
            bail!("return outside an inline function")
        };
        // The frame is off the stack only while the result is stored: a `finally` body run
        // below may itself `return` from this frame.
        let stored = self.store_inline_result(ctx, &mut frame, expression);
        let done = frame.done;
        self.inline_returns.push(frame);
        stored?;
        self.run_finallies(ctx, |_| true)?;
        self.jump(ctx, done);
        self.after_terminator(ctx);
        Ok(())
    }

    /// Store a returned value in the function's slot, created by the first typed return.
    /// An untyped `return;` before that is recorded and patched once the slot exists.
    fn store_inline_result(
        &mut self,
        ctx: &mut Context,
        frame: &mut ReturnFrame,
        expression: Option<&'s Expr>,
    ) -> Result<()> {
        if frame.slot.is_none() && matches!(expression, None | Some(Expr::Undefined)) {
            frame.empty_returns.push(self.block);
            let finally = self.new_block(ctx, "inline_empty_return");
            self.jump(ctx, finally);
            self.switch_to(finally);
            return Ok(());
        }
        let existing = frame
            .slot
            .map(|slot| self.slot_type(ctx, slot).map(|ty| (slot, ty)))
            .transpose()?;
        let expected = existing.map(|(_, ty)| ty);
        let value = match (expression, expected) {
            (Some(expression), _) => self.expression_with(ctx, expression, expected)?,
            (None, Some(ty)) => self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)),
            (None, None) => {
                let ty = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))
            }
        };
        let (slot, ty) = match existing {
            Some(existing) => existing,
            None => {
                let value_type = value.get_type(ctx);
                let ty = if frame.optional {
                    OptionalType::get(ctx, value_type).into()
                } else {
                    value_type
                };
                let slot = self.entry_slot(ctx, ty);
                frame.slot = Some(slot);
                (slot, ty)
            }
        };
        let value = self.fit(ctx, value, ty)?;
        self.store(ctx, slot, value);
        Ok(())
    }
}

// How a statement list may complete, as a bit set.
const FALLTHROUGH: u8 = 1;
const RETURN: u8 = 2;
const BREAK: u8 = 4;
const CONTINUE: u8 = 8;
const LABELLED: u8 = 16;
// Return-type inference also sees syntactically present returns on unreachable paths.
const EMPTY_RETURN: u8 = 32;

/// Compute the completion mask of `statements` so a callback that can fall through yields
/// `undefined` on that path.
fn completions(statements: &[Stmt]) -> u8 {
    let mut result = FALLTHROUGH;
    for statement in statements {
        let next = match statement {
            Stmt::Return(None | Some(Expr::Undefined)) => RETURN | EMPTY_RETURN,
            Stmt::Return(_) | Stmt::Throw(_) => RETURN,
            Stmt::Break => BREAK,
            Stmt::Continue => CONTINUE,
            Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => LABELLED,
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                completions(then_branch)
                    | else_branch
                        .as_ref()
                        .map_or(FALLTHROUGH, |body| completions(body))
            }
            Stmt::Switch { cases, .. } => {
                let mut tail = FALLTHROUGH;
                let mut branches = if cases.iter().any(|case| case.test.is_none()) {
                    0
                } else {
                    FALLTHROUGH
                };
                for case in cases.iter().rev() {
                    let body = completions(&case.body);
                    tail = (body & !FALLTHROUGH) | if body & FALLTHROUGH != 0 { tail } else { 0 };
                    branches |= tail;
                }
                (branches & !BREAK)
                    | if branches & BREAK != 0 {
                        FALLTHROUGH
                    } else {
                        0
                    }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                let body = completions(body);
                (body & (RETURN | EMPTY_RETURN | LABELLED))
                    | if matches!(condition, Expr::Bool(true)) && body & BREAK == 0 {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::For {
                condition, body, ..
            } => {
                let body = completions(body);
                let inf = condition
                    .as_ref()
                    .is_none_or(|c| matches!(c, Expr::Bool(true)));
                (body & (RETURN | EMPTY_RETURN | LABELLED))
                    | if inf && body & BREAK == 0 {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                let body =
                    completions(body) | catch.as_ref().map_or(0, |catch| completions(&catch.body));
                let final_state = finally
                    .as_ref()
                    .map_or(FALLTHROUGH, |body| completions(body));
                (body & EMPTY_RETURN)
                    | (final_state & !FALLTHROUGH)
                    | if final_state & FALLTHROUGH != 0 {
                        body
                    } else {
                        0
                    }
            }
            Stmt::Labeled { body, .. } => {
                FALLTHROUGH | (completions(std::slice::from_ref(body)) & EMPTY_RETURN)
            }
            _ => FALLTHROUGH,
        };
        result = (result & !FALLTHROUGH)
            | if result & FALLTHROUGH != 0 {
                next
            } else {
                next & EMPTY_RETURN
            };
    }
    result
}

pub(super) fn returns_optional(statements: &[Stmt]) -> bool {
    completions(statements) & (FALLTHROUGH | EMPTY_RETURN) != 0
}
