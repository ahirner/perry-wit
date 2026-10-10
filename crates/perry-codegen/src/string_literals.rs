//! Comparisons against short string constants without a runtime call.
use crate::{FunctionLowering, i32_type};
use perry_mir::{attrs::CompareKind, collections::StringConstOp};
use perry_runtime::string::{DATA, LEN};
use pliron::{
    context::{Context, Ptr},
    operation::Operation,
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_> {
    pub(super) fn string_literal_comparison(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        operands: &[Value],
        kind: CompareKind,
    ) -> Option<Value> {
        for constant in [1, 0] {
            let Some(definition) = op.deref(ctx).get_operand(constant).defining_op() else {
                continue;
            };
            let Some(literal) = Operation::get_op::<StringConstOp>(definition, ctx) else {
                continue;
            };
            let equality = matches!(kind, CompareKind::Eq | CompareKind::Ne);
            let (bytes, length) = {
                let text = literal.get_attr_perry_string_value(ctx)?;
                let text: &str = text.as_ref();
                if !(equality && text.len() <= 8 || text.len() == 1 && text.is_ascii()) {
                    continue;
                }
                let mut bytes = [0; 8];
                bytes[..text.len()].copy_from_slice(text.as_bytes());
                (bytes, text.len())
            };
            let string = operands[1 - constant];
            let size = self.load(ctx, MemKind::I32, string, LEN);
            let expected = self.i32_const(ctx, length as i32);
            if equality {
                let same_length = self.num(ctx, NumOpcode::I32Eq, vec![size, expected]);
                let equal = if length == 0 {
                    same_length
                } else {
                    let ty = i32_type(ctx);
                    let join = self.new_block(ctx, vec![ty]);
                    let compare = self.new_block(ctx, vec![]);
                    let no = self.i32_const(ctx, 0);
                    self.cond_br(ctx, same_length, (compare, vec![]), (join, vec![no]));
                    self.block = compare;
                    let data = self.load(ctx, MemKind::I32, string, DATA);
                    let mut offset = 0;
                    let mut equal = self.i32_const(ctx, 1);
                    for (width, memory) in [
                        (8, MemKind::I64),
                        (4, MemKind::I32),
                        (2, MemKind::I32U16),
                        (1, MemKind::I32U8),
                    ] {
                        if length - offset < width {
                            continue;
                        }
                        let loaded = self.load(ctx, memory, data, offset as u32);
                        let mut word = [0; 8];
                        word[..width].copy_from_slice(&bytes[offset..offset + width]);
                        let value = u64::from_le_bytes(word);
                        let (expected, opcode) = if width == 8 {
                            (self.i64_const(ctx, value), NumOpcode::I64Eq)
                        } else {
                            (self.i32_const(ctx, value as i32), NumOpcode::I32Eq)
                        };
                        let same = self.num(ctx, opcode, vec![loaded, expected]);
                        equal = self.num(ctx, NumOpcode::I32And, vec![equal, same]);
                        offset += width;
                    }
                    self.br(ctx, join, vec![equal]);
                    self.block = join;
                    join.deref(ctx).get_argument(0)
                };
                return Some(if kind == CompareKind::Ne {
                    self.num(ctx, NumOpcode::I32Eqz, vec![equal])
                } else {
                    equal
                });
            }
            // For a one-byte ASCII literal, UTF-8 byte ordering decides the first scalar.
            // A matching first byte is equal only when the variable also ends there.
            let ty = i32_type(ctx);
            let join = self.new_block(ctx, vec![ty]);
            let compare = self.new_block(ctx, vec![]);
            let less = self.i32_const(ctx, -1);
            self.cond_br(ctx, size, (compare, vec![]), (join, vec![less]));
            self.block = compare;
            let data = self.load(ctx, MemKind::I32, string, DATA);
            let first = self.load(ctx, MemKind::I32U8, data, 0);
            let byte = self.i32_const(ctx, i32::from(bytes[0]));
            let difference = self.num(ctx, NumOpcode::I32Sub, vec![first, byte]);
            let tail = self.num(ctx, NumOpcode::I32Ne, vec![size, expected]);
            let order = self.select(ctx, difference, difference, tail);
            self.br(ctx, join, vec![order]);
            self.block = join;
            let order = join.deref(ctx).get_argument(0);
            let zero = self.i32_const(ctx, 0);
            let opcode = match (kind, constant == 0) {
                (CompareKind::Lt, false) | (CompareKind::Gt, true) => NumOpcode::I32LtS,
                (CompareKind::Le, false) | (CompareKind::Ge, true) => NumOpcode::I32LeS,
                (CompareKind::Gt, false) | (CompareKind::Lt, true) => NumOpcode::I32GtS,
                (CompareKind::Ge, false) | (CompareKind::Le, true) => NumOpcode::I32GeS,
                _ => unreachable!(),
            };
            return Some(self.num(ctx, opcode, vec![order, zero]));
        }
        None
    }
}
