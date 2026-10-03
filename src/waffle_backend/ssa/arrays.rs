//! Bounds and receiver validation for arrays of inline string descriptors.

use anyhow::{Result, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{BlockTarget, MemoryArg, Operator, Terminator, Type, Value};

use super::FunctionLowerer;
use crate::waffle_backend::strings::valid_index;

impl FunctionLowerer<'_> {
    pub(super) fn array_receiver(&mut self, expr: &Expr) -> Result<Value> {
        ensure!(
            self.infer_expr_type(expr) == HirType::Array(Box::new(HirType::String)),
            "Expected a string-array receiver, got {:?}",
            self.infer_expr_type(expr)
        );
        self.expression(expr)
    }

    pub(super) fn array_index(&mut self, object: &Expr, index: &Expr) -> Result<Value> {
        let array = self.array_receiver(object)?;
        let index = self.position_argument(Some(index), f64::NAN)?;
        let count = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 4,
                    memory: self.registry.memory,
                },
            },
            &[array],
            &[Type::I32],
        );
        let valid = valid_index(&mut self.body, self.block, index, count);
        let present = self.body.add_block();
        let join = self.body.add_block();
        let result = self.body.add_blockparam(join, Type::I32);
        let undefined = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: valid,
                if_true: BlockTarget {
                    block: present,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: join,
                    args: vec![undefined],
                },
            },
        );
        self.block = present;
        let elements = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 0,
                    memory: self.registry.memory,
                },
            },
            &[array],
            &[Type::I32],
        );
        let index = self.op(Operator::I32TruncF64U, &[index], &[Type::I32]);
        let stride = self.op(Operator::I32Const { value: 12 }, &[], &[Type::I32]);
        let offset = self.op(Operator::I32Mul, &[index, stride], &[Type::I32]);
        let descriptor = self.op(Operator::I32Add, &[elements, offset], &[Type::I32]);
        self.branch(join, vec![descriptor]);
        self.block = join;
        Ok(result)
    }
}
