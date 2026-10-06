//! Dense array construction, bounds checks, and typed element access.

use anyhow::{Result, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{BlockTarget, MemoryArg, Operator, Terminator, Type, Value};

use super::FunctionLowerer;
use crate::waffle_backend::strings::valid_index;

/// An immediate in-bounds literal index cannot observe the temporary array's identity.
/// Keep the full item list so emission still evaluates unselected elements in order.
pub(super) fn literal_projection<'a>(
    object: &'a Expr,
    index: &Expr,
) -> Option<(&'a [Expr], usize)> {
    let Expr::Array(items) = object else {
        return None;
    };
    let index = match index {
        Expr::Number(value) => *value,
        Expr::Integer(value) => *value as f64,
        _ => return None,
    };
    (index >= 0.0 && index.fract() == 0.0 && index < items.len() as f64)
        .then_some((items, index as usize))
}

impl FunctionLowerer<'_> {
    pub(super) fn project_values<'e>(
        &mut self,
        items: impl Iterator<Item = &'e Expr>,
        selected: usize,
    ) -> Result<Value> {
        let mut result = None;
        for (index, item) in items.enumerate() {
            let value = self.expression(item)?;
            if index == selected {
                result = Some(value);
            }
        }
        Ok(result.expect("literal projection validated the index"))
    }

    pub(super) fn is_dense_array(&self, expression: &Expr) -> bool {
        matches!(self.infer_expr_type(expression),HirType::Array(inner) if *inner!=HirType::String)
    }

    pub(super) fn dense_index(&mut self, object: &Expr, index: &Expr) -> Result<Value> {
        let HirType::Array(inner) = self.infer_expr_type(object) else {
            unreachable!()
        };
        ensure!(
            self.infer_expr_type(index) == HirType::Number,
            "Array indices must be numbers"
        );
        let array = self.expression(object)?;
        let index = self.expression(index)?;
        let memory = MemoryArg {
            memory: self.registry.memory,
            offset: 4,
            align: 2,
        };
        let count = self.op(Operator::I32Load { memory }, &[array], &[Type::I32]);
        let address = self.checked_array_slot(array, index, count, 4);
        let value = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    offset: 0,
                    ..memory
                },
            },
            &[address],
            &[Type::I32],
        );
        self.extract_value(value, &inner)
    }

    pub(super) fn dense_set(
        &mut self,
        object: &Expr,
        index: &Expr,
        expression: &Expr,
    ) -> Result<Value> {
        let HirType::Array(inner) = self.infer_expr_type(object) else {
            unreachable!()
        };
        ensure!(
            self.infer_expr_type(index) == HirType::Number,
            "Array indices must be numbers"
        );
        let array = self.expression(object)?;
        let index = self.expression(index)?;
        let value = self.typed_operand(expression, &inner)?;
        let boxed = self.box_typed_value(value, &inner)?;
        let memory = MemoryArg {
            memory: self.registry.memory,
            offset: 4,
            align: 2,
        };
        let count = self.op(Operator::I32Load { memory }, &[array], &[Type::I32]);
        let address = self.checked_array_slot(array, index, count, 4);
        self.op(
            Operator::I32Store {
                memory: MemoryArg {
                    offset: 0,
                    ..memory
                },
            },
            &[address, boxed],
            &[],
        );
        Ok(value)
    }

    pub(super) fn dense_push(&mut self, object: &Expr, expression: &Expr) -> Result<Value> {
        ensure!(
            self.is_dense_array(object),
            "push requires a declared dense array"
        );
        let HirType::Array(inner) = self.infer_expr_type(object) else {
            unreachable!()
        };
        let array = self.expression(object)?;
        let value = self.typed_operand(expression, &inner)?;
        let boxed = self.box_typed_value(value, &inner)?;
        let memory = MemoryArg {
            memory: self.registry.memory,
            offset: 4,
            align: 2,
        };
        let count = self.op(Operator::I32Load { memory }, &[array], &[Type::I32]);
        let one = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let next = self.op(Operator::I32Add, &[count, one], &[Type::I32]);
        self.op(
            Operator::Call {
                function_index: self.registry.value_access.unwrap().array_resize,
            },
            &[array, next],
            &[],
        );
        let index = self.op(Operator::F64ConvertI32U, &[count], &[Type::F64]);
        let address = self.checked_array_slot(array, index, next, 4);
        self.op(
            Operator::I32Store {
                memory: MemoryArg {
                    offset: 0,
                    ..memory
                },
            },
            &[address, boxed],
            &[],
        );
        Ok(self.op(Operator::F64ConvertI32U, &[next], &[Type::F64]))
    }

    pub(super) fn checked_array_slot(
        &mut self,
        array: Value,
        index: Value,
        count: Value,
        stride: u32,
    ) -> Value {
        let valid = valid_index(&mut self.body, self.block, index, count);
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
        let data = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    memory: self.registry.memory,
                    offset: 0,
                    align: 2,
                },
            },
            &[array],
            &[Type::I32],
        );
        let index = self.op(Operator::I32TruncF64U, &[index], &[Type::I32]);
        let stride = self.op(Operator::I32Const { value: stride }, &[], &[Type::I32]);
        let offset = self.op(Operator::I32Mul, &[index, stride], &[Type::I32]);
        self.op(Operator::I32Add, &[data, offset], &[Type::I32])
    }

    pub(super) fn new_string_array(&mut self, items: &[Expr]) -> Result<Value> {
        let size = u32::try_from(items.len())?
            .checked_mul(8)
            .ok_or_else(|| anyhow::anyhow!("String array is too large"))?;
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let alignment = self.op(Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let size = self.op(Operator::I32Const { value: size.max(1) }, &[], &[Type::I32]);
        let data = self.op(
            Operator::Call {
                function_index: self.registry.allocator.unwrap().realloc,
            },
            &[zero, zero, alignment, size],
            &[Type::I32],
        );
        self.op(
            Operator::MemoryFill {
                mem: self.registry.memory,
            },
            &[data, zero, size],
            &[],
        );
        let count = self.op(
            Operator::I32Const {
                value: items.len().try_into()?,
            },
            &[],
            &[Type::I32],
        );
        let value = self.op(
            Operator::Call {
                function_index: self.registry.structured_helpers.unwrap().lift_strings,
            },
            &[data, count],
            &[Type::I32],
        );
        self.reference_values.insert(value);
        let elements = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    memory: self.registry.memory,
                    offset: 0,
                    align: 2,
                },
            },
            &[value],
            &[Type::I32],
        );
        for (index, item) in items.iter().enumerate() {
            let string = self.string_receiver(item)?;
            for offset in [0, 4, 8] {
                let memory = MemoryArg {
                    memory: self.registry.memory,
                    offset,
                    align: 2,
                };
                let field = self.op(Operator::I32Load { memory }, &[string], &[Type::I32]);
                self.op(
                    Operator::I32Store {
                        memory: MemoryArg {
                            offset: index as u32 * 12 + offset,
                            ..memory
                        },
                    },
                    &[elements, field],
                    &[],
                );
            }
        }
        Ok(value)
    }

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
