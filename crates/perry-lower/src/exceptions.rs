//! Exception edges and structured completion through finally blocks.
use super::*;
use perry_hir::ir::CatchClause;
use perry_mir::attrs::ErrorKind;
use std::rc::Rc;

#[derive(Clone, Copy)]
pub(super) struct Handler {
    block: Ptr<BasicBlock>,
    status: Value,
}

#[derive(Clone)]
pub(super) struct Cleanup {
    body: Rc<[Stmt]>,
    handler_depth: usize,
    loop_depth: usize,
}

impl FunctionBuilder<'_> {
    pub(super) fn guard_exception(&mut self, ctx: &mut Context, op: &dyn Op) {
        if self.handlers.is_empty()
            || !op_cast::<dyn MayThrowInterface>(op).is_some_and(|op| op.may_throw(ctx))
            || op.get_operation().deref(ctx).get_num_successors() != 0
            || self.terminated(ctx)
        {
            return;
        }
        let status = self.emit_value(ctx, CaughtStatusOp::new);
        let handler = *self.handlers.last().unwrap();
        self.store(ctx, handler.status, status);
        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
        let zero = ConstantOp::new(
            ctx,
            Box::new(IntegerAttr::new(ty, APInt::from_u64(0, bw(32)))),
        );
        let zero = self.append_value(ctx, &zero);
        let failed = self.compare(ctx, CompareKind::Ne, status, zero);
        let normal = self.new_block(ctx, "normal");
        self.branch(ctx, failed, handler.block, normal);
        self.switch_to(normal);
    }

    pub(super) fn box_exception(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        match kind(ctx, value)? {
            Kind::Exception => Ok(value),
            Kind::Number | Kind::Boolean | Kind::String => {
                Ok(self.emit_value(ctx, |ctx| BoxExceptionOp::new(ctx, value)))
            }
            _ => bail!("throwing this type is not supported yet; use an Error or primitive"),
        }
    }

    pub(super) fn new_error(
        &mut self,
        ctx: &mut Context,
        kind: ErrorKind,
        message: Option<&Expr>,
    ) -> Result<Value> {
        let message = match message {
            Some(expr) => {
                let value = self.expression(ctx, expr)?;
                self.defined(ctx, value)?
            }
            None => self.emit_value(ctx, |ctx| StringConstOp::new(ctx, "")),
        };
        ensure!(
            message.get_type(ctx).deref(ctx).is::<StringType>(),
            "Error messages must be strings"
        );
        Ok(self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, kind, message)))
    }

    pub(super) fn propagate(&mut self, ctx: &mut Context, status: Value) {
        if let Some(handler) = self.handlers.last().copied() {
            self.store(ctx, handler.status, status);
            self.jump(ctx, handler.block);
        } else {
            self.emit(ctx, |ctx| RethrowOp::new(ctx, status));
        }
    }

    /// Inline finalizers for a return or loop exit. Each executes in its lexical handler and
    /// loop scope; an abrupt finalizer replaces the pending completion.
    pub(super) fn run_cleanups(&mut self, ctx: &mut Context, depth: usize) -> Result<()> {
        let saved_cleanups = self.cleanups.clone();
        let saved_handlers = self.handlers.clone();
        let saved_loops = self.loops.clone();
        let result = (|| {
            for index in (depth..saved_cleanups.len()).rev() {
                let cleanup = &saved_cleanups[index];
                self.cleanups.truncate(index);
                self.handlers.truncate(cleanup.handler_depth);
                self.loops.truncate(cleanup.loop_depth);
                self.statements(ctx, &cleanup.body)?;
                if self.terminated(ctx) {
                    break;
                }
            }
            Ok(())
        })();
        self.cleanups = saved_cleanups;
        self.handlers = saved_handlers;
        self.loops = saved_loops;
        result
    }

    pub(super) fn try_statement(
        &mut self,
        ctx: &mut Context,
        body: &[Stmt],
        catch: Option<&CatchClause>,
        finally: Option<&[Stmt]>,
    ) -> Result<()> {
        let depth = self.cleanups.len();
        let outer_handlers = self.handlers.len();
        let join = self.new_block(ctx, "try_done");
        let mut handler = |this: &mut Self, label| {
            let block = this.new_block(ctx, label);
            let ty = IntegerType::get(ctx, 32, Signedness::Signless).into();
            let status = this.entry_slot(ctx, ty);
            Handler { block, status }
        };
        let caught = catch.map(|_| handler(self, "catch"));
        let cleanup_throw = finally.map(|_| handler(self, "finally_throw"));
        if let Some(body) = finally {
            self.cleanups.push(Cleanup {
                body: Rc::from(body),
                handler_depth: outer_handlers,
                loop_depth: self.loops.len(),
            });
        }
        if let Some(handler) = caught.or(cleanup_throw) {
            self.handlers.push(handler);
        }
        self.statements(ctx, body)?;
        self.handlers.truncate(outer_handlers);
        if !self.terminated(ctx) {
            self.run_cleanups(ctx, depth)?;
            self.jump(ctx, join);
        }

        if let (Some(catch), Some(handler)) = (catch, caught) {
            self.switch_to(handler.block);
            let status = self.load(ctx, handler.status)?;
            let exception = self.emit_value(ctx, |ctx| CatchOp::new(ctx, status));
            if let Some((id, _)) = &catch.param {
                let slot = self.entry_slot(ctx, exception.get_type(ctx));
                self.store(ctx, slot, exception);
                self.locals.insert(*id, slot);
            }
            if let Some(handler) = cleanup_throw {
                self.handlers.push(handler);
            }
            self.statements(ctx, &catch.body)?;
            self.handlers.truncate(outer_handlers);
            if !self.terminated(ctx) {
                self.run_cleanups(ctx, depth)?;
                self.jump(ctx, join);
            }
        }

        if let Some(handler) = cleanup_throw {
            self.switch_to(handler.block);
            let status = self.load(ctx, handler.status)?;
            // Clear the pending exception while the finalizer runs, retaining its identity.
            let exception = self.emit_value(ctx, |ctx| CatchOp::new(ctx, status));
            self.run_cleanups(ctx, depth)?;
            if !self.terminated(ctx) {
                let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, exception));
                self.propagate(ctx, status);
            }
        }
        self.cleanups.truncate(depth);
        self.switch_to(join);
        Ok(())
    }
}
