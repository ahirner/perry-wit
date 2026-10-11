//! Inline string comparisons, character reads and owned single-byte appends.
use crate::{FunctionLowering, Runtime, i32_type};
use perry_mir::{attrs::CompareKind, collections::StringConstOp};
use perry_runtime::string::{BUFFER, CURSOR_BYTE, CURSOR_SCALAR, DATA, LEN, SCALARS, SIZE};
use pliron::{
    context::{Context, Ptr},
    operation::Operation,
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

#[derive(Clone, Copy)]
pub(super) enum Character {
    String,
    Scalar,
}

impl FunctionLowering<'_, '_> {
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
    /// `string.charAt(position)`: an integral in-range position of ASCII text selects a static
    /// one-character string inline, as does an ASCII scalar of mixed text read in order (at or
    /// just after the runtime's scan cursor, which this path advances). Other cases call the
    /// runtime.
    pub(super) fn char_at(
        &mut self,
        ctx: &mut Context,
        string: Value,
        position: Value,
        representation: Character,
    ) -> Value {
        // Saturating conversion: NaN and negatives give 0, so require an exact round trip.
        let index = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![position]);
        let back = self.num(ctx, NumOpcode::F64ConvertI32U, vec![index]);
        let integral = self.num(ctx, NumOpcode::F64Eq, vec![back, position]);
        let scalars = self.load(ctx, MemKind::I32, string, SCALARS);
        // Against the scalar count: the mixed-text path scans scalars, not bytes.
        let inside = self.num(ctx, NumOpcode::I32LtU, vec![index, scalars]);
        let valid = self.num(ctx, NumOpcode::I32And, vec![integral, inside]);
        self.character(ctx, string, index, position, valid, representation)
    }

    pub(super) fn char_at_index(
        &mut self,
        ctx: &mut Context,
        string: Value,
        index: Value,
        representation: Character,
    ) -> Value {
        let position = self.num(ctx, NumOpcode::F64ConvertI32U, vec![index]);
        let valid = self.i32_const(ctx, 1);
        self.character(ctx, string, index, position, valid, representation)
    }

    /// The character at scalar `index` (`position` as a number for the runtime fallback);
    /// `valid` tells whether `index` is an in-range integer.
    fn character(
        &mut self,
        ctx: &mut Context,
        string: Value,
        index: Value,
        position: Value,
        valid: Value,
        representation: Character,
    ) -> Value {
        let i32 = crate::i32_type(ctx);
        let join = self.new_block(ctx, vec![i32]);
        let inline = self.new_block(ctx, vec![]);
        let call = self.new_block(ctx, vec![]);
        let length = self.load(ctx, MemKind::I32, string, LEN);
        let scalars = self.load(ctx, MemKind::I32, string, SCALARS);
        let ascii = self.num(ctx, NumOpcode::I32Eq, vec![length, scalars]);
        let mixed = self.new_block(ctx, vec![]);
        let sequential = self.new_block(ctx, vec![]);
        let scanned = self.new_block(ctx, vec![]);
        let ascii_valid = self.num(ctx, NumOpcode::I32And, vec![ascii, valid]);
        self.cond_br(ctx, ascii_valid, (inline, vec![]), (mixed, vec![]));
        self.block = inline;
        let data = self.load(ctx, MemKind::I32, string, DATA);
        let address = self.num(ctx, NumOpcode::I32Add, vec![data, index]);
        let byte = self.load(ctx, MemKind::I32U8, address, 0);
        let character = self.character_value(ctx, representation, byte);
        self.br(ctx, join, vec![character]);
        // Mixed text read in order: the scan cursor names the previous (or this) scalar.
        self.block = mixed;
        let cursor = self.load(ctx, MemKind::I32, string, CURSOR_SCALAR);
        let one = self.i32_const(ctx, 1);
        let next = self.num(ctx, NumOpcode::I32Add, vec![cursor, one]);
        let at_cursor = self.num(ctx, NumOpcode::I32Eq, vec![index, cursor]);
        let after = self.num(ctx, NumOpcode::I32Eq, vec![index, next]);
        let near = self.num(ctx, NumOpcode::I32Or, vec![at_cursor, after]);
        let near = self.num(ctx, NumOpcode::I32And, vec![near, valid]);
        self.cond_br(ctx, near, (sequential, vec![]), (call, vec![]));
        self.block = sequential;
        let data = self.load(ctx, MemKind::I32, string, DATA);
        let start = self.load(ctx, MemKind::I32, string, CURSOR_BYTE);
        let address = self.num(ctx, NumOpcode::I32Add, vec![data, start]);
        let lead = self.load(ctx, MemKind::I32U8, address, 0);
        // UTF-8 sequence length from its lead byte.
        let (two, three, four) = (
            self.i32_const(ctx, 2),
            self.i32_const(ctx, 3),
            self.i32_const(ctx, 4),
        );
        let (limit1, limit2, limit3) = (
            self.i32_const(ctx, 0x80),
            self.i32_const(ctx, 0xe0),
            self.i32_const(ctx, 0xf0),
        );
        let below3 = self.num(ctx, NumOpcode::I32LtU, vec![lead, limit3]);
        let step = self.select(ctx, below3, three, four);
        let below2 = self.num(ctx, NumOpcode::I32LtU, vec![lead, limit2]);
        let step = self.select(ctx, below2, two, step);
        let below1 = self.num(ctx, NumOpcode::I32LtU, vec![lead, limit1]);
        let step = self.select(ctx, below1, one, step);
        let zero = self.i32_const(ctx, 0);
        let step = self.select(ctx, after, step, zero);
        let offset = self.num(ctx, NumOpcode::I32Add, vec![start, step]);
        let address = self.num(ctx, NumOpcode::I32Add, vec![data, offset]);
        let byte = self.load(ctx, MemKind::I32U8, address, 0);
        match representation {
            Character::String => {
                let is_ascii = self.num(ctx, NumOpcode::I32LtU, vec![byte, limit1]);
                self.cond_br(ctx, is_ascii, (scanned, vec![]), (call, vec![]));
            }
            Character::Scalar => self.br(ctx, scanned, vec![]),
        }
        self.block = scanned;
        self.store(ctx, MemKind::I32, string, CURSOR_SCALAR, index);
        self.store(ctx, MemKind::I32, string, CURSOR_BYTE, offset);
        let character = match representation {
            Character::String => self.character_value(ctx, representation, byte),
            Character::Scalar => self.scalar_at_byte(ctx, address, byte),
        };
        self.br(ctx, join, vec![character]);
        self.block = call;
        let runtime = match representation {
            Character::String => Runtime::StdStringCharAt,
            Character::Scalar => Runtime::StdStringScalarAt,
        };
        let character = self.runtime(ctx, runtime, vec![string, position]);
        self.br(ctx, join, vec![character]);
        self.block = join;
        join.deref(ctx).get_argument(0)
    }

    /// Decode one valid UTF-8 scalar at a proven scalar boundary. Loads stop at its width.
    fn scalar_at_byte(&mut self, ctx: &mut Context, address: Value, first: Value) -> Value {
        let ty = i32_type(ctx);
        let join = self.new_block(ctx, vec![ty]);
        let multibyte = self.new_block(ctx, vec![]);
        let limit = self.i32_const(ctx, 0x80);
        let ascii = self.num(ctx, NumOpcode::I32LtU, vec![first, limit]);
        self.cond_br(ctx, ascii, (join, vec![first]), (multibyte, vec![]));
        self.block = multibyte;
        let e0 = self.i32_const(ctx, 0xe0);
        let f0 = self.i32_const(ctx, 0xf0);
        let two_bytes = self.num(ctx, NumOpcode::I32LtU, vec![first, e0]);
        let three_bytes = self.num(ctx, NumOpcode::I32LtU, vec![first, f0]);
        let mask2 = self.i32_const(ctx, 0x1f);
        let mask3 = self.i32_const(ctx, 0x0f);
        let mask4 = self.i32_const(ctx, 0x07);
        let mask = self.select(ctx, three_bytes, mask3, mask4);
        let mask = self.select(ctx, two_bytes, mask2, mask);
        let mut scalar = self.num(ctx, NumOpcode::I32And, vec![first, mask]);
        let six = self.i32_const(ctx, 6);
        let mask = self.i32_const(ctx, 0x3f);
        for offset in 1..=3 {
            let byte = self.load(ctx, MemKind::I32U8, address, offset);
            let bits = self.num(ctx, NumOpcode::I32And, vec![byte, mask]);
            let high = self.num(ctx, NumOpcode::I32Shl, vec![scalar, six]);
            scalar = self.num(ctx, NumOpcode::I32Or, vec![high, bits]);
            if offset < 3 {
                let next = self.new_block(ctx, vec![]);
                let complete = if offset == 1 { two_bytes } else { three_bytes };
                self.cond_br(ctx, complete, (join, vec![scalar]), (next, vec![]));
                self.block = next;
            }
        }
        self.br(ctx, join, vec![scalar]);
        self.block = join;
        join.deref(ctx).get_argument(0)
    }

    /// Append one byte to an exclusively owned header's spare tail capacity. Failure leaves
    /// the string untouched; MIR supplies the allocating fallback and its collection point.
    pub(super) fn try_string_append(
        &mut self,
        ctx: &mut Context,
        a: Value,
        b: Value,
        representation: Character,
    ) -> Value {
        let i32 = crate::i32_type(ctx);
        let join = self.new_block(ctx, vec![i32]);
        let buffered = self.new_block(ctx, vec![]);
        let inline = self.new_block(ctx, vec![]);
        let fail = self.new_block(ctx, vec![]);
        let buffer = self.load(ctx, MemKind::I32, a, BUFFER);
        let one = self.i32_const(ctx, 1);
        let zero = self.i32_const(ctx, 0);
        let single = match representation {
            Character::String => {
                let suffix = self.load(ctx, MemKind::I32, b, LEN);
                self.num(ctx, NumOpcode::I32Eq, vec![suffix, one])
            }
            Character::Scalar => {
                let limit = self.i32_const(ctx, 128);
                self.num(ctx, NumOpcode::I32LtU, vec![b, limit])
            }
        };
        let owned = self.num(ctx, NumOpcode::I32Ne, vec![buffer, zero]);
        let candidate = self.num(ctx, NumOpcode::I32And, vec![single, owned]);
        self.cond_br(ctx, candidate, (buffered, vec![]), (fail, vec![]));
        self.block = buffered;
        // Buffer header: capacity, then bytes used; the bytes follow it.
        let capacity = self.load(ctx, MemKind::I32, buffer, 0);
        let used = self.load(ctx, MemKind::I32, buffer, 4);
        let data = self.load(ctx, MemKind::I32, a, DATA);
        let length = self.load(ctx, MemKind::I32, a, LEN);
        let end = self.num(ctx, NumOpcode::I32Add, vec![data, length]);
        let eight = self.i32_const(ctx, 8);
        let bytes = self.num(ctx, NumOpcode::I32Add, vec![buffer, eight]);
        let tail = self.num(ctx, NumOpcode::I32Add, vec![bytes, used]);
        let at_end = self.num(ctx, NumOpcode::I32Eq, vec![end, tail]);
        let room = self.num(ctx, NumOpcode::I32LtU, vec![used, capacity]);
        let fits = self.num(ctx, NumOpcode::I32And, vec![at_end, room]);
        self.cond_br(ctx, fits, (inline, vec![]), (fail, vec![]));
        self.block = inline;
        let byte = match representation {
            Character::String => {
                let source = self.load(ctx, MemKind::I32, b, DATA);
                self.load(ctx, MemKind::I32U8, source, 0)
            }
            Character::Scalar => b,
        };
        self.store(ctx, MemKind::I32U8, tail, 0, byte);
        let one = self.i32_const(ctx, 1);
        let used = self.num(ctx, NumOpcode::I32Add, vec![used, one]);
        self.store(ctx, MemKind::I32, buffer, 4, used);
        let length = self.num(ctx, NumOpcode::I32Add, vec![length, one]);
        self.store(ctx, MemKind::I32, a, LEN, length);
        let scalars = self.load(ctx, MemKind::I32, a, SCALARS);
        let scalars = self.num(ctx, NumOpcode::I32Add, vec![scalars, one]);
        self.store(ctx, MemKind::I32, a, SCALARS, scalars);
        self.br(ctx, join, vec![one]);
        self.block = fail;
        let no = self.i32_const(ctx, 0);
        self.br(ctx, join, vec![no]);
        self.block = join;
        join.deref(ctx).get_argument(0)
    }

    /// Materialize ASCII only when MIR needs a string; scalar users keep the byte.
    fn character_value(
        &mut self,
        ctx: &mut Context,
        representation: Character,
        byte: Value,
    ) -> Value {
        if matches!(representation, Character::Scalar) {
            return byte;
        }
        let table = match self.ascii_strings {
            Some(table) => table,
            None => {
                let table = self.entry_runtime(ctx, Runtime::StdAsciiStrings);
                self.ascii_strings = Some(table);
                table
            }
        };
        let size = self.i32_const(ctx, SIZE as i32);
        let offset = self.num(ctx, NumOpcode::I32Mul, vec![byte, size]);
        self.num(ctx, NumOpcode::I32Add, vec![table, offset])
    }
}
