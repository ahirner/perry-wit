//! Short-circuit boolean expressions with branch-local effects and joined locals.

use super::FunctionLowerer;
use crate::waffle_backend::control_flow::JoinPoint;
use anyhow::{Context, Result, ensure};
use perry_hir::{
    ir::{Expr, LogicalOp},
    types::Type as HirType,
};
use std::mem;
use waffle::{BlockTarget, Terminator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn boolean_logic(
        &mut self,
        operation: LogicalOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<Value> {
        ensure!(
            matches!(operation, LogicalOp::And | LogicalOp::Or)
                && self.infer_expr_type(left) == HirType::Boolean
                && self.infer_expr_type(right) == HirType::Boolean,
            "Logical operators require statically boolean operands; implicit value selection is unsupported"
        );
        let left_value = self.expression(left)?;
        let incoming_narrowings = self.narrowings.clone();
        let evaluate_right = self.body.add_block();
        let join = JoinPoint::new(&mut self.body, "boolean join", &self.locals);
        let result = self.body.add_blockparam(join.block, Type::I32);
        let mut shortcut = join.branch_args(&self.locals);
        shortcut.push(left_value);
        let shortcut = BlockTarget {
            block: join.block,
            args: shortcut,
        };
        let evaluate = BlockTarget {
            block: evaluate_right,
            args: vec![],
        };
        let (if_true, if_false) = if operation == LogicalOp::And {
            (evaluate, shortcut)
        } else {
            (shortcut, evaluate)
        };
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: left_value,
                if_true,
                if_false,
            },
        );
        self.block = evaluate_right;
        self.narrow_type_guard(left, operation == LogicalOp::And);
        let right_value = self.expression(right)?;
        let mut arguments = join.branch_args(&self.locals);
        arguments.push(right_value);
        self.branch(join.block, arguments);
        self.block = join.block;
        self.locals = join.bindings;
        self.narrowings
            .retain(|id, ty| incoming_narrowings.get(id) == Some(ty));
        Ok(result)
    }
}

impl FunctionLowerer<'_> {
    pub(super) fn conditional(
        &mut self,
        condition: &Expr,
        then_expr: &Expr,
        else_expr: &Expr,
    ) -> Result<Value> {
        let ty = self.conditional_type(then_expr, else_expr).context(
            "Conditional expressions require a boolean condition and matching static branch types",
        )?;
        ensure!(
            self.infer_expr_type(condition) == HirType::Boolean,
            "Conditional expressions require a boolean condition and matching static branch types"
        );
        let core = crate::waffle_backend::registry::map_type_to_waffle(&ty)?;
        let value = self.expression(condition)?;
        let incoming_narrowings = mem::take(&mut self.narrowings);
        let then_block = self.body.add_block();
        let else_block = self.body.add_block();
        let join = JoinPoint::new(&mut self.body, "conditional join", &self.locals);
        let result = self.body.add_blockparam(join.block, core);
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: value,
                if_true: BlockTarget {
                    block: then_block,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: else_block,
                    args: vec![],
                },
            },
        );
        let branch_locals = [self.locals.clone(), mem::take(&mut self.locals)];
        for ((block, expression, truth), locals) in [
            (then_block, then_expr, true),
            (else_block, else_expr, false),
        ]
        .into_iter()
        .zip(branch_locals)
        {
            self.block = block;
            self.locals = locals;
            self.narrowings = incoming_narrowings.clone();
            self.narrow_type_guard(condition, truth);
            let value = self.typed_operand(expression, &ty)?;
            let mut arguments = join.branch_args(&self.locals);
            arguments.push(value);
            self.branch(join.block, arguments);
        }
        self.block = join.block;
        self.locals = join.bindings;
        self.narrowings = incoming_narrowings;
        Ok(result)
    }
}
