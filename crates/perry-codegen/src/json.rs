//! Shared native layout descriptors and JSON writing.
mod decode;
use crate::{
    function::FunctionLowering,
    repr::{Carriers, optional_inner, slot_size},
    runtime::Runtime,
};
use anyhow::{Result, anyhow, ensure};
use perry_abi::layout::descriptor::{
    JSON_DATE, JSON_DICTIONARY, JSON_NUMBER_STRING, JSON_NUMERIC, JSON_TUPLE, JSON_VALUE,
    JSON_VARIANT, NULL_ALLOWED, UNDEFINED_ALLOWED,
};
use perry_abi::layout::record;
use perry_mir::{
    ArrayType, DateType, ExceptionType, JsonSchema, JsonType, OptionalType, RecordType, StringType,
};
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

    pub(crate) fn layout_descriptor(
        &mut self,
        ctx: &Context,
        ty: TypeHandle,
        json: bool,
    ) -> Result<u32> {
        use perry_abi::layout::descriptor::{
            ARRAY, BOOLEAN, EXCEPTION, NUMBER, OPTIONAL, RECORD, SIGNED_INT, STRING, UNSIGNED_INT,
        };
        if let Some(&address) = if json {
            self.state.json_descriptors.get(&ty)
        } else {
            self.state.native_descriptors.get(&ty)
        } {
            return Ok(address);
        }
        if json && let Some(inner) = optional_inner(ctx, ty) {
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
            return Ok(self.layout_descriptor(ctx, inner, true)?
                | (u32::from(absence.admits_undefined()) * UNDEFINED_ALLOWED)
                | (u32::from(absence.admits_null()) * NULL_ALLOWED));
        }
        let definition = ty.deref(ctx);
        let record_fields = definition
            .downcast_ref::<RecordType>()
            .map(RecordType::fields)
            .transpose()
            .map_err(|_| anyhow!("unresolved native record"))?;
        let count = record_fields.map_or(0, <[_]>::len);
        let bytes = if record_fields.is_some() {
            24 + 12 * count + if json { 4 + 8 * count } else { 0 }
        } else if let Some(variant) = definition.downcast_ref::<perry_mir::VariantType>() {
            16 + 12 * variant.cases().len()
        } else if definition.is::<OptionalType>() {
            8
        } else if definition.is::<perry_mir::NumberStringType>() {
            16
        } else if definition.is::<ArrayType>() {
            12
        } else {
            8
        };
        let address = self.state.data.append(4, &vec![0; bytes]);
        if json {
            self.state.json_descriptors.insert(ty, address);
        } else {
            self.state.native_descriptors.insert(ty, address);
        }
        let words = if let Some(fields) = record_fields {
            let mut words = Vec::with_capacity(bytes / 4);
            let tuple = TypedHandle::<RecordType>::from_handle(ty, ctx)?
                .deref(ctx)
                .is_tuple();
            words.extend([
                if json && tuple { JSON_TUPLE } else { RECORD },
                count as u32,
            ]);
            let mut field_offset = record::HEADER;
            for (name, field_ty) in fields.iter() {
                let name = self.state.data.string(name);
                let field = self.layout_descriptor(ctx, *field_ty, json)?;
                words.extend([name, field, field_offset]);
                field_offset += slot_size(ctx, *field_ty);
            }
            let offsets: Vec<_> = fields
                .iter()
                .scan(record::HEADER, |offset, (_, ty)| {
                    let current = *offset;
                    *offset += slot_size(ctx, *ty);
                    Some((current, *ty))
                })
                .filter_map(|(offset, ty)| {
                    (crate::repr::repr(ctx, ty).ok() == Some(crate::repr::Repr::Ref))
                        .then_some(offset)
                })
                .chain(std::iter::once(record::JSON))
                .collect();
            let shape = self.pointer_shape(&offsets);
            let (table, _) = self.json_field_table(ctx, ty)?;
            let required = fields
                .iter()
                .filter(|(_, ty)| {
                    !perry_mir::admits_undefined(ctx, *ty) && !ty.deref(ctx).is::<JsonType>()
                })
                .count() as u32;
            words.extend([field_offset, shape, table, required]);
            if json {
                let mut numeric: Vec<_> = fields
                    .iter()
                    .enumerate()
                    .filter_map(|(index, (name, _))| {
                        perry_abi::json::array_index(name.as_bytes()).map(|number| (number, index))
                    })
                    .collect();
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
            }
            words
        } else if let Some(variant) = definition.downcast_ref::<perry_mir::VariantType>() {
            ensure!(json, "unsupported native cause field type");
            let mut words = vec![JSON_VARIANT, variant.cases().len() as u32];
            for (name, payload) in variant.cases() {
                ensure!(
                    payload.is_none_or(|ty| slot_size(ctx, ty) <= 8),
                    "tagged numeric variant payload exceeds its native slot"
                );
                let name = self.state.data.string(name);
                let child = payload
                    .map(|ty| self.layout_descriptor(ctx, ty, json))
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
            words.extend([shape, table]);
            words
        } else if let Some(inner) = optional_inner(ctx, ty) {
            vec![OPTIONAL, self.layout_descriptor(ctx, inner, false)?]
        } else if let Ok(array) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            let element = array.deref(ctx).element();
            let kind = Self::array_element_kind(ctx, element)? as u32;
            let element = self.layout_descriptor(ctx, element, json)?;
            let dictionary = matches!(JsonSchema::of(ctx, ty), Some(JsonSchema::Dictionary { .. }));
            vec![
                if json && dictionary {
                    JSON_DICTIONARY
                } else {
                    ARRAY
                },
                element,
                kind,
            ]
        } else {
            ensure!(
                json || !ty.deref(ctx).is::<perry_mir::NumberStringType>()
                    && !ty.deref(ctx).is::<perry_mir::NumericType>()
                    && !ty.deref(ctx).is::<JsonType>(),
                "unsupported native cause field type"
            );
            let kind = if ty.deref(ctx).is::<perry_mir::NumberStringType>() {
                JSON_NUMBER_STRING
            } else if ty.deref(ctx).is::<perry_mir::NumericType>() {
                JSON_NUMERIC
            } else if ty.deref(ctx).is::<JsonType>() {
                JSON_VALUE
            } else if json && ty.deref(ctx).is::<DateType>() {
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
            if kind == JSON_NUMBER_STRING {
                vec![
                    kind,
                    self.pointer_shape(&[
                        perry_abi::layout::variant::OWNER,
                        perry_abi::layout::variant::JSON,
                    ]),
                    self.state.data.string("number"),
                    self.state.data.string("string"),
                ]
            } else {
                vec![kind, width]
            }
        };
        let offset = (address - crate::DATA_BASE) as usize;
        for (index, word) in words.into_iter().enumerate() {
            self.state.data.bytes[offset + index * 4..offset + index * 4 + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
        Ok(address)
    }

    pub(super) fn json_stringify(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        let input = op.deref(ctx).get_operand(0);
        let value = self.mapped_value(ctx, input)?;
        let ty = input.get_type(ctx);
        let schema = self.layout_descriptor(ctx, ty, true)?;
        let schema = self.i32_const(ctx, schema as i32);
        let (bits, tag) = match value {
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
        };
        let data = if let Some(data) = self.state.json_write_data {
            data
        } else {
            let mut bytes = b"nulltruefalse{}:\"".to_vec();
            bytes.resize(20, 0);
            for text in ["\"tag\":", "\"val\":", "tag", "val"] {
                bytes.extend_from_slice(&self.state.data.string(text).to_le_bytes());
            }
            bytes.extend_from_slice(&u32::from(self.state.retains_json).to_le_bytes());
            bytes.resize(72, 0);
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
