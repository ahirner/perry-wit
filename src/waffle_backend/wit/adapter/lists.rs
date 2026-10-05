//! WIT lists reuse typed array storage and checked canonical element layouts.

use super::*;

impl Adapter<'_> {
    pub(super) fn lift_list(&mut self, element: Type, input: &mut Input<'_>) -> Result<Value> {
        let (data, count) = match input {
            Input::Flat { values, cursor } => {
                let data = self.convert_flat(values[*cursor], CoreType::I32)?;
                let count = self.convert_flat(values[*cursor + 1], CoreType::I32)?;
                *cursor += 2;
                (data, count)
            }
            Input::Memory { pointer, offset } => (
                self.load_i32(*pointer, *offset),
                self.load_i32(*pointer, *offset + 4),
            ),
        };
        if element == Type::U8 {
            return Ok(self.call(
                self.registry.byte_helpers.unwrap().lift_canonical,
                &[data, count],
            ));
        }
        if self.alias(element) == Type::String {
            return Ok(self.call(
                self.registry.structured_helpers.unwrap().lift_strings,
                &[data, count],
            ));
        }
        let array = self.call(self.registry.value_access.unwrap().array_new, &[count]);
        let target = self.load_i32(array, 0);
        let stride = self.sizes.size(&element).size_wasm32() as u32;
        self.list_loop(count, |adapter, index| {
            let pointer = adapter.list_address(data, index, stride);
            let value = adapter.lift(element, &mut Input::Memory { pointer, offset: 0 })?;
            let boxed = adapter.box_value(element, value)?;
            let slot = adapter.list_address(target, index, 4);
            adapter.store_i32(slot, 0, boxed);
            Ok(())
        })?;
        Ok(array)
    }

    pub(super) fn lower_list(
        &mut self,
        element: Type,
        array: Value,
        pointer: Value,
        offset: u32,
        source_type: &HirType,
    ) -> Result<()> {
        if element == Type::U8 {
            let mask = self.integer(!1);
            let array = self.op(Operator::I32And, &[array, mask], CoreType::I32);
            let data = self.load_i32(array, 0);
            let count = self.load_i32(array, 4);
            self.store_i32(pointer, offset, data);
            self.store_i32(pointer, offset + 4, count);
            return Ok(());
        }
        if self.alias(element) == Type::String
            || crate::waffle_backend::structured::is_string_array(source_type)
        {
            let list = self.call(
                self.registry.structured_helpers.unwrap().lower_strings,
                &[array],
            );
            let data = self.load_i32(list, 0);
            let count = self.load_i32(list, 4);
            self.store_i32(pointer, offset, data);
            self.store_i32(pointer, offset + 4, count);
            return Ok(());
        }
        let HirType::Array(child_type) = source_type else {
            bail!("WIT list needs a source array")
        };
        let count = self.load_i32(array, 4);
        let source = self.load_i32(array, 0);
        let stride = self.sizes.size(&element).size_wasm32() as u32;
        let alignment = self.sizes.align(&element).align_wasm32() as u32;
        let width = self.op(
            Operator::I64Const {
                value: stride as u64,
            },
            &[],
            CoreType::I64,
        );
        let length = self.op(Operator::I64ExtendI32U, &[count], CoreType::I64);
        let size = self.op(Operator::I64Mul, &[width, length], CoreType::I64);
        let maximum = self.op(
            Operator::I64Const {
                value: u32::MAX as u64,
            },
            &[],
            CoreType::I64,
        );
        let valid = self.op(Operator::I64LeU, &[size, maximum], CoreType::I32);
        self.require(valid);
        let size = self.op(Operator::I32WrapI64, &[size], CoreType::I32);
        let zero = self.integer(0);
        let alignment = self.integer(alignment);
        let data = self.call(
            self.registry.allocator.unwrap().realloc,
            &[zero, zero, alignment, size],
        );
        self.store_i32(pointer, offset, data);
        self.store_i32(pointer, offset + 4, count);
        self.list_loop(count, |adapter, index| {
            let slot = adapter.list_address(source, index, 4);
            let boxed = adapter.load_i32(slot, 0);
            let value = adapter.extract_outbound(element, boxed, child_type)?;
            let pointer = adapter.list_address(data, index, stride);
            adapter.lower(element, value, pointer, 0, child_type)
        })
    }

    fn list_address(&mut self, data: Value, index: Value, stride: u32) -> Value {
        let stride = self.integer(stride);
        let offset = self.op(Operator::I32Mul, &[index, stride], CoreType::I32);
        self.op(Operator::I32Add, &[data, offset], CoreType::I32)
    }

    fn list_loop(
        &mut self,
        count: Value,
        element: impl FnOnce(&mut Self, Value) -> Result<()>,
    ) -> Result<()> {
        let head = self.body.add_block();
        let body = self.body.add_block();
        let exit = self.body.add_block();
        let index = self.body.add_blockparam(head, CoreType::I32);
        let zero = self.integer(0);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: head,
                    args: vec![zero],
                },
            },
        );
        self.block = head;
        let more = self.op(Operator::I32LtU, &[index, count], CoreType::I32);
        self.body.set_terminator(
            head,
            Terminator::CondBr {
                cond: more,
                if_true: BlockTarget {
                    block: body,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: exit,
                    args: vec![],
                },
            },
        );
        self.block = body;
        element(self, index)?;
        let one = self.integer(1);
        let next = self.op(Operator::I32Add, &[index, one], CoreType::I32);
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget {
                    block: head,
                    args: vec![next],
                },
            },
        );
        self.block = exit;
        Ok(())
    }
}
