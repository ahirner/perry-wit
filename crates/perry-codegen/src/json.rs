//! Shared native layout descriptors and JSON writing.
mod decode;
use crate::{
    function::FunctionLowering,
    repr::{Carriers, optional_inner, slot_size},
    runtime::Runtime,
};
use anyhow::{Result, anyhow, ensure};
use perry_abi::layout::descriptor::{
    JSON_DATE, JSON_DICTIONARY, JSON_NUMERIC, JSON_TUPLE, JSON_VALUE, JSON_VARIANT, NULL_ALLOWED,
    UNDEFINED_ALLOWED,
};
use perry_abi::layout::{descriptor, record};
use perry_mir::{ArrayType, DateType, ExceptionType, JsonSchema, JsonType, RecordType, StringType};
use pliron::{
    builtin::types::{FP64Type, IntegerType},
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
};
use pliron_wasm::attrs::NumOpcode;

/// The member prefix `"name":` of a record field known at compile time, quoted as ECMAScript
/// `QuoteJSONString` does.
fn json_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 3);
    out.push('"');
    for character in name.chars() {
        match u8::try_from(character) {
            // A Latin-1 scalar is its byte; the escaper keeps such bytes or emits ASCII.
            Ok(byte) => perry_abi::json::quote_contents(&[byte], |part| {
                out.extend(part.iter().copied().map(char::from));
            }),
            // Only ASCII needs an escape.
            Err(_) => out.push(character),
        }
    }
    out.push_str("\":");
    out
}

impl FunctionLowering<'_, '_> {
    /// Share the indexed name table between cursor decoding and retained record graphs.
    pub(super) fn json_field_table(
        &mut self,
        ctx: &Context,
        ty: TypeHandle,
    ) -> Result<(u32, usize)> {
        if let Some(&table) = self.state.json_fields.get(&ty) {
            return Ok(table);
        }
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx)?;
        let record = record.deref(ctx);
        let mut names = record.fields()?.iter().enumerate().collect::<Vec<_>>();
        names.sort_unstable_by(|(_, (a, _)), (_, (b, _))| {
            a.len()
                .cmp(&b.len())
                .then_with(|| a.as_bytes().cmp(b.as_bytes()))
        });
        let mut bytes = Vec::with_capacity(names.len() * 8);
        for (index, (name, _)) in &names {
            bytes.extend_from_slice(&self.state.data.string(name).to_le_bytes());
            bytes.extend_from_slice(&(*index as u32).to_le_bytes());
        }
        let table = (self.state.data.append(4, &bytes), names.len());
        self.state.json_fields.insert(ty, table);
        Ok(table)
    }

    pub(crate) fn layout_descriptor(&mut self, ctx: &Context, ty: TypeHandle) -> Result<u32> {
        use perry_abi::layout::descriptor::{
            ARRAY, BOOLEAN, EXCEPTION, NUMBER, RECORD, SIGNED_INT, STRING, UNSIGNED_INT,
        };
        if let Some(&address) = self.state.layout_descriptors.get(&ty) {
            return Ok(address);
        }
        if let Some(inner) = optional_inner(ctx, ty) {
            ensure!(
                crate::values::is_numeric_type(ctx, inner)
                    || matches!(
                        crate::repr::repr(ctx, inner)?,
                        crate::repr::Repr::F64 | crate::repr::Repr::Bool | crate::repr::Repr::Ref
                    ),
                "optional integer lacks sentinel representation"
            );
            let absence = perry_mir::absence(ctx, ty)
                .ok_or_else(|| anyhow!("optional descriptor without absence states"))?;
            let address = self.layout_descriptor(ctx, inner)?
                | (u32::from(absence.admits_undefined()) * UNDEFINED_ALLOWED)
                | (u32::from(absence.admits_null()) * NULL_ALLOWED);
            self.state.layout_descriptors.insert(ty, address);
            return Ok(address);
        }
        let definition = ty.deref(ctx);
        let variant_schema = match JsonSchema::of(ctx, ty) {
            Some(JsonSchema::Variant(variant) | JsonSchema::ValueUnion(variant)) => Some(variant),
            _ => None,
        };
        let record = definition.downcast_ref::<RecordType>();
        let record_fields = record
            .map(|r| Ok::<_, anyhow::Error>((r.is_tuple(), r.fields()?)))
            .transpose()
            .map_err(|_| anyhow!("unresolved native record"))?;
        let count = record_fields.map_or(0, |(_, fields)| fields.len());
        let bytes = if record_fields.is_some() {
            28 + 20 * count
        } else if let Some(variant) = variant_schema {
            20 + 12 * variant.deref(ctx).cases().len()
        } else if definition.is::<ArrayType>() {
            12
        } else {
            8
        };
        let address = self.state.data.append(4, &vec![0; bytes]);
        self.state.layout_descriptors.insert(ty, address);
        let words = if let Some((tuple, fields)) = record_fields {
            let mut words = Vec::with_capacity(bytes / 4);
            words.extend([if tuple { JSON_TUPLE } else { RECORD }, count as u32]);
            let mut field_offset = record::HEADER;
            let mut offsets = Vec::with_capacity(count + 1);
            let mut required = 0u32;
            let mut numeric = Vec::new();
            for (index, (name, field_ty)) in fields.iter().enumerate() {
                let field_ty = *field_ty;
                let name_addr = self.state.data.string(name);
                let field = self.layout_descriptor(ctx, field_ty)?;
                words.extend([name_addr, field, field_offset]);
                if crate::repr::repr(ctx, field_ty).ok() == Some(crate::repr::Repr::Ref) {
                    offsets.push(field_offset);
                }
                if !perry_mir::admits_undefined(ctx, field_ty)
                    && !field_ty.deref(ctx).is::<JsonType>()
                {
                    required += 1;
                }
                if let Some(number) = perry_abi::json::array_index(name.as_bytes()) {
                    numeric.push((number, index));
                }
                field_offset += slot_size(ctx, field_ty);
            }
            offsets.push(record::JSON);
            let shape = self.pointer_shape(&offsets);
            let (table, _) = self.json_field_table(ctx, ty)?;
            words.extend([field_offset, shape, table, required]);
            numeric.sort_unstable();
            let mut next = vec![u32::MAX; count];
            for (position, &(_, index)) in numeric.iter().enumerate() {
                next[index] = numeric
                    .get(position + 1)
                    .map_or(0, |&(_, index)| index as u32 + 1);
            }
            words.push(numeric.first().map_or(0, |&(_, index)| index as u32 + 1));
            for ((name, _), next) in fields.iter().zip(next) {
                words.extend([self.state.data.string(&json_key(name)), next]);
            }
            words
        } else if let Some(variant) = variant_schema {
            let variant = variant.deref(ctx);
            let kind = if definition.is::<perry_mir::ValueUnionType>() {
                descriptor::JSON_VALUE_UNION
            } else {
                JSON_VARIANT
            };
            let mut words = vec![kind, variant.cases().len() as u32];
            for (name, payload) in variant.cases() {
                ensure!(
                    payload.is_none_or(|ty| slot_size(ctx, ty) <= 8),
                    "tagged numeric variant payload exceeds its native slot"
                );
                let name = self.state.data.string(name);
                let child = payload
                    .map(|ty| self.layout_descriptor(ctx, ty))
                    .transpose()?
                    .unwrap_or(0);
                let reference = payload.is_some_and(|ty| {
                    crate::repr::repr(ctx, ty).ok() == Some(crate::repr::Repr::Ref)
                });
                words.extend([name, child, u32::from(reference)]);
            }
            let shape = self.pointer_shape(&[
                perry_abi::layout::variant::OWNER,
                perry_abi::layout::variant::JSON,
            ]);
            let mut table = Vec::with_capacity(16);
            for (index, name) in ["tag", "val"].into_iter().enumerate() {
                table.extend_from_slice(&self.state.data.string(name).to_le_bytes());
                table.extend_from_slice(&(index as u32).to_le_bytes());
            }
            let table = self.state.data.append(4, &table);
            words.extend([shape, table, u32::from(self.state.retains_json)]);
            words
        } else if let Ok(array) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            let element = array.deref(ctx).element();
            let kind = Self::array_element_kind(ctx, element)? as u32;
            let element = self.layout_descriptor(ctx, element)?;
            let dictionary = matches!(JsonSchema::of(ctx, ty), Some(JsonSchema::Dictionary { .. }));
            vec![
                if dictionary { JSON_DICTIONARY } else { ARRAY },
                element,
                kind,
            ]
        } else {
            let kind = if ty.deref(ctx).is::<perry_mir::NumericType>() {
                JSON_NUMERIC
            } else if ty.deref(ctx).is::<JsonType>() {
                JSON_VALUE
            } else if ty.deref(ctx).is::<perry_mir::BigIntType>() {
                descriptor::JSON_BIGINT
            } else if ty.deref(ctx).is::<DateType>() {
                JSON_DATE
            } else if ty.deref(ctx).is::<FP64Type>() {
                NUMBER
            } else if let Some(int) = ty.deref(ctx).downcast_ref::<IntegerType>() {
                if int.width() == 1 {
                    BOOLEAN
                } else if int.signedness() == pliron::builtin::types::Signedness::Unsigned {
                    UNSIGNED_INT
                } else {
                    SIGNED_INT
                }
            } else if ty.deref(ctx).is::<StringType>() {
                STRING
            } else if ty.deref(ctx).is::<ExceptionType>() {
                EXCEPTION
            } else {
                return Err(anyhow!("unsupported native cause field type"));
            };
            let width = ty
                .deref(ctx)
                .downcast_ref::<IntegerType>()
                .map_or(0, |ty| ty.width());
            vec![kind, width]
        };
        self.write_data_words(address, words);
        Ok(address)
    }

    pub(super) fn write_data_words(&mut self, address: u32, words: Vec<u32>) {
        let offset = (address - crate::DATA_BASE) as usize;
        for (index, word) in words.into_iter().enumerate() {
            self.state.data.bytes[offset + index * 4..offset + index * 4 + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
    }

    pub(super) fn json_carrier_bits(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        value: Carriers<pliron::value::Value>,
    ) -> (pliron::value::Value, pliron::value::Value) {
        match value {
            Carriers::Numeric { tag, payload } => (payload, tag),
            Carriers::Scalar(value) => {
                let bits = if matches!(crate::numeric::integer(ctx, ty), Some((width, true)) if width <= 32)
                {
                    self.num(ctx, NumOpcode::I64ExtendI32S, vec![value])
                } else {
                    self.slot_bits(ctx, value)
                };
                (bits, self.i32_const(ctx, 0))
            }
        }
    }

    pub(super) fn json_stringify(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        let input = op.deref(ctx).get_operand(0);
        let value = self.mapped_value(ctx, input)?;
        let ty = input.get_type(ctx);
        let schema = self.layout_descriptor(ctx, ty)?;
        let schema = self.i32_const(ctx, schema as i32);
        let (bits, tag) = self.json_carrier_bits(ctx, ty, value);
        if Operation::get_op::<perry_mir::JsonBoxOp>(op, ctx).is_some() {
            let value = self.runtime(ctx, Runtime::StdJsonBox, vec![bits, tag, schema]);
            self.define(ctx, op, value);
            return Ok(());
        }
        let data = if let Some(data) = self.state.json_write_data {
            data
        } else {
            use descriptor::*;
            let mut bytes = [0; WRITE_SIZE as usize];
            for (offset, literal) in [
                (WRITE_NULL, "null"),
                (WRITE_TRUE, "true"),
                (WRITE_FALSE, "false"),
                (WRITE_EMPTY_OBJECT, "{}"),
                (WRITE_COLON, ":"),
                (WRITE_QUOTE, "\""),
            ] {
                let offset = offset as usize;
                bytes[offset..offset + literal.len()].copy_from_slice(literal.as_bytes());
            }
            for (offset, word) in [
                (WRITE_TAG_KEY, self.state.data.string("\"tag\":")),
                (WRITE_VALUE_KEY, self.state.data.string("\"val\":")),
                (WRITE_TAG_NAME, self.state.data.string("tag")),
                (WRITE_VALUE_NAME, self.state.data.string("val")),
                (WRITE_RETAINS_JSON, u32::from(self.state.retains_json)),
            ] {
                let offset = offset as usize;
                bytes[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
            }
            let data = self.state.data.append(8, &bytes);
            self.state.json_write_data = Some(data);
            data
        };
        let data = self.i32_const(ctx, data as i32);
        let text = self.runtime(
            ctx,
            Runtime::JsonDescriptorStringify,
            vec![schema, bits, tag, data],
        );
        let status = self.runtime(ctx, Runtime::Status, vec![]);
        self.throw_status_if(ctx, status, status);
        self.define(ctx, op, text);
        Ok(())
    }
}
