//! Fixed tuples retain declared element types and cannot grow or acquire holes.

use super::FunctionLowerer;
use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{BlockTarget, MemoryArg, Operator, Terminator, Type, Value};

pub(super) fn element_type(types: &[HirType], index: &Expr) -> Result<HirType> {
    let constant = match index {
        Expr::Number(value) => Some(*value),
        Expr::Integer(value) => Some(*value as f64),
        _ => None,
    };
    if let Some(index) = constant {
        ensure!(
            index >= 0.0 && index.fract() == 0.0 && index < types.len() as f64,
            "Tuple index is outside its declared bounds"
        );
        return Ok(types[index as usize].clone());
    }
    let Some(first) = types.first() else {
        bail!("Cannot index an empty tuple");
    };
    ensure!(
        types.iter().all(|ty| ty == first),
        "A mixed tuple requires a constant index"
    );
    Ok(first.clone())
}

impl FunctionLowerer<'_> {
    pub(super) fn tuple_index(
        &mut self,
        object: &Expr,
        index: &Expr,
        types: &[HirType],
    ) -> Result<Value> {
        let ty = element_type(types, index)?;
        ensure!(
            self.infer_expr_type(index) == HirType::Number,
            "Tuple indices must be numbers"
        );
        let array = self.expression(object)?;
        let index = self.expression(index)?;
        let count = self.op(
            Operator::I32Const {
                value: types.len() as u32,
            },
            &[],
            &[Type::I32],
        );
        let valid =
            crate::waffle_backend::strings::valid_index(&mut self.body, self.block, index, count);
        let present = self.body.add_block();
        let absent = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: valid,
                if_true: BlockTarget {
                    block: present,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: absent,
                    args: vec![],
                },
            },
        );
        self.block = absent;
        let error = self.op(
            Operator::F64Const {
                value: 12f64.to_bits(),
            },
            &[],
            &[Type::F64],
        );
        self.emit_throw(error);
        self.block = present;
        let memory = MemoryArg {
            memory: self.registry.memory,
            offset: 0,
            align: 2,
        };
        let data = self.op(Operator::I32Load { memory }, &[array], &[Type::I32]);
        let index = self.op(Operator::I32TruncF64U, &[index], &[Type::I32]);
        let stride = self.op(Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let offset = self.op(Operator::I32Mul, &[index, stride], &[Type::I32]);
        let address = self.op(Operator::I32Add, &[data, offset], &[Type::I32]);
        let boxed = self.op(Operator::I32Load { memory }, &[address], &[Type::I32]);
        self.extract_value(boxed, &ty)
    }
}
