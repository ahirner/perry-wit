//! Compact WIT flags use optional boolean record fields at the source boundary.

use super::*;

pub(super) fn storage_type(flags: &wit_parser::Flags) -> Type {
    match flags.flags.len() {
        0..=8 => Type::U8,
        9..=16 => Type::U16,
        _ => Type::U32,
    }
}

impl Adapter<'_> {
    pub(super) fn lift_flags(
        &mut self,
        flags: &wit_parser::Flags,
        input: &mut Input<'_>,
    ) -> Result<Value> {
        let bits = self.read_scalar(storage_type(flags), input)?;
        let object = self.call(self.registry.object_helpers.unwrap().new, &[]);
        let zero = self.integer(0);
        for (index, flag) in flags.flags.iter().enumerate() {
            let mask = self.integer(1 << index);
            let value = self.op(Operator::I32And, &[bits, mask], CoreType::I32);
            let value = self.op(Operator::I32Ne, &[value, zero], CoreType::I32);
            self.set_field(object, &to_camel_case(&flag.name), Type::Bool, value)?;
        }
        Ok(object)
    }

    pub(super) fn lower_flags(
        &mut self,
        flags: &wit_parser::Flags,
        object: Value,
        pointer: Value,
        offset: u32,
    ) -> Result<()> {
        let mut bits = self.integer(0);
        for (index, flag) in flags.flags.iter().enumerate() {
            let key = self.integer(self.strings.get(&to_camel_case(&flag.name)).unwrap());
            let entry = self.call(self.registry.object_helpers.unwrap().get, &[object, key]);
            let tag = self.integer(ValueTag::Boolean as u32);
            let optional = self.integer(1);
            let payload = self.call_checked(
                self.registry.object_helpers.unwrap().value,
                &[entry, tag, optional],
            );
            let value = abi::decode_payload(&mut self.body, self.block, payload, true);
            let shift = self.integer(index as u32);
            let value = self.op(Operator::I32Shl, &[value, shift], CoreType::I32);
            bits = self.op(Operator::I32Or, &[bits, value], CoreType::I32);
        }
        let memory = self.memory(offset);
        let operator = match storage_type(flags) {
            Type::U8 => Operator::I32Store8 { memory },
            Type::U16 => Operator::I32Store16 { memory },
            _ => Operator::I32Store { memory },
        };
        self.body
            .add_op(self.block, operator, &[pointer, bits], &[]);
        Ok(())
    }
}
