//! Checked construction of WIT 64-bit integers from exactly representable numbers.
use super::*;
impl FunctionLowerer<'_> {
    pub(super) fn number_to_bigint(&mut self, input: &Expr) -> Result<Value> {
        let ty = self.infer_expr_type(input);
        if ty == HirType::BigInt {
            return self.expression(input);
        }
        ensure!(
            ty == HirType::Number,
            "BigInt construction supports safe integer numbers and transported bigint values"
        );
        let number = self.expression(input)?;
        let rounded = self.op(Operator::F64Trunc, &[number], &[Type::F64]);
        let whole = self.op(Operator::F64Eq, &[number, rounded], &[Type::I32]);
        let max = self.op(
            Operator::F64Const {
                value: 9007199254740991f64.to_bits(),
            },
            &[],
            &[Type::F64],
        );
        let magnitude = self.op(Operator::F64Abs, &[number], &[Type::F64]);
        let safe = self.op(Operator::F64Le, &[magnitude, max], &[Type::I32]);
        let valid = self.op(Operator::I32And, &[whole, safe], &[Type::I32]);
        let success = self.body.add_block();
        let failure = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: valid,
                if_true: BlockTarget {
                    block: success,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: failure,
                    args: vec![],
                },
            },
        );
        self.block = failure;
        let reason = self.op(
            Operator::F64Const {
                value: 1f64.to_bits(),
            },
            &[],
            &[Type::F64],
        );
        self.emit_native_throw(reason);
        self.block = success;
        let integer = self.op(Operator::I64TruncF64S, &[number], &[Type::I64]);
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let eight = self.op(Operator::I32Const { value: 8 }, &[], &[Type::I32]);
        let pointer = self.op(
            Operator::Call {
                function_index: self.registry.allocator.unwrap().realloc,
            },
            &[zero, zero, eight, eight],
            &[Type::I32],
        );
        self.op(
            Operator::I64Store {
                memory: MemoryArg {
                    memory: self.registry.memory,
                    offset: 0,
                    align: 3,
                },
            },
            &[pointer, integer],
            &[],
        );
        Ok(pointer)
    }
}
