//! Loop branches preserve local values and cross only the required finally clauses.

use anyhow::{Result, anyhow};
use perry_hir::ir::{Expr, Stmt};
use waffle::{BlockTarget, Operator, Terminator, Type};

use super::FunctionLowerer;
use crate::waffle_backend::{
    control_flow::JoinPoint,
    exceptions::{self, ExitReason},
};

pub(super) struct LoopScope {
    exit: JoinPoint,
    step: JoinPoint,
    enclosing_try_depth: usize,
}

impl FunctionLowerer<'_> {
    pub(super) fn loop_statement(
        &mut self,
        condition: Option<&Expr>,
        statements: &[Stmt],
        update: Option<&Expr>,
    ) -> Result<()> {
        self.invalidate_narrowings(statements);
        for expression in condition.into_iter().chain(update) {
            self.invalidate_narrowings(&[Stmt::Expr(expression.clone())]);
        }
        let incoming_narrowings = self.narrowings.clone();
        let guard = condition;
        let header = JoinPoint::new(&mut self.body, "loop header", &self.locals);
        header.emit_branch(&mut self.body, self.block, &self.locals);
        self.block = header.block;
        self.locals = header.bindings.clone();

        let condition = match condition {
            Some(expr) => self.condition(expr)?,
            None => self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]),
        };
        let body = self.body.add_block();
        let exit = JoinPoint::new(&mut self.body, "loop exit", &self.locals);
        let step = JoinPoint::new(&mut self.body, "loop step", &self.locals);
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: condition,
                if_true: BlockTarget {
                    block: body,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: exit.block,
                    args: exit.branch_args(&self.locals),
                },
            },
        );
        self.loops.push(LoopScope {
            exit,
            step,
            enclosing_try_depth: self.unwind_ctx.depth(),
        });
        self.block = body;
        if let Some(guard) = guard {
            self.narrow_type_guard(guard, true);
        }
        self.statements(statements)?;
        let scope = self.loops.pop().expect("Active loop scope");
        if self.body.blocks[self.block].terminator == Terminator::None {
            scope
                .step
                .emit_branch(&mut self.body, self.block, &self.locals);
        }

        self.block = scope.step.block;
        self.locals = scope.step.bindings;
        self.narrowings = incoming_narrowings.clone();
        if self.body.blocks[self.block].preds.is_empty() {
            self.body
                .set_terminator(self.block, Terminator::Unreachable);
        } else {
            if let Some(update) = update {
                self.expression(update)?;
            }
            self.collection_blocks.insert(self.block);
            header.emit_branch(&mut self.body, self.block, &self.locals);
        }
        self.block = scope.exit.block;
        self.locals = scope.exit.bindings;
        self.narrowings = incoming_narrowings;
        Ok(())
    }

    pub(super) fn loop_exit(&mut self, reason: ExitReason) -> Result<()> {
        let scope = self
            .loops
            .last()
            .ok_or_else(|| anyhow!("{reason:?} requires an enclosing loop"))?;
        let zero = self.body.add_op(
            self.block,
            Operator::F64Const { value: 0 },
            &[],
            &[Type::F64],
        );
        if exceptions::route_cleanup(
            &mut self.body,
            self.block,
            self.unwind_ctx.target_for_exit(scope.enclosing_try_depth),
            &self.locals,
            (reason, zero),
        ) {
            let target = match reason {
                ExitReason::Break => &scope.exit,
                ExitReason::Continue => &scope.step,
                _ => unreachable!("Loop exits are break or continue"),
            };
            target.emit_branch(&mut self.body, self.block, &self.locals);
        }
        Ok(())
    }
}
