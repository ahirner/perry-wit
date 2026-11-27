//! Shared native layout descriptors and JSON writing.
mod decode;
use crate::{
    function::FunctionLowering,
    module::StaticTable,
    repr::{Carriers, optional_inner, slot_size},
    runtime::Runtime,
};
use perry_abi::layout::descriptor;
use perry_abi::layout::descriptor::{
    JSON_DICTIONARY, JSON_TUPLE, JSON_VARIANT, NULL_ALLOWED, UNDEFINED_ALLOWED,
};
use perry_mir::{ArrayType, JsonSchema, JsonType, PlironResultExt as _, RecordType};
use perry_wit_diagnostics::{Diagnostic, Result, ensure_internal, unsupported};
use pliron::{
    builtin::types::IntegerType,
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

impl crate::module::Data {
    pub(super) fn field_names<'a>(&mut self, names: impl Iterator<Item = &'a str>) -> u32 {
        let mut fields: Vec<_> = names.enumerate().collect();
        fields.sort_unstable_by(|(_, a), (_, b)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        let bytes = fields
            .into_iter()
            .flat_map(|(index, name)| [self.string(name), index as u32])
            .flat_map(u32::to_le_bytes)
            .collect();
        self.constant(4, bytes)
    }
}

impl FunctionLowering<'_, '_> {
    /// Share the indexed name table between cursor decoding and retained record graphs.
    pub(super) fn json_field_table(
        &mut self,
        ctx: &Context,
        ty: TypeHandle,
    ) -> Result<(u32, usize)> {
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx).to_diag(ctx)?;
        let layout = self.state.record_layouts.get(ctx, record)?;
        Ok((
            self.state
                .static_table(ctx, StaticTable::FieldNames(record))?,
            layout.fields.len(),
        ))
    }

    pub(crate) fn layout_descriptor(&mut self, ctx: &Context, ty: TypeHandle) -> Result<u32> {
        use perry_abi::layout::descriptor::{ARRAY, RECORD};
        if let Some(&address) = self.state.layout_descriptors.get(&ty) {
            return Ok(address);
        }
        if let Some(inner) = optional_inner(ctx, ty) {
            ensure_internal!(
                crate::values::is_numeric_type(ctx, inner)
                    || matches!(
                        crate::repr::repr(ctx, inner)?,
                        crate::repr::Repr::F64 | crate::repr::Repr::Bool | crate::repr::Repr::Ref
                    ),
                "optional integer lacks sentinel representation"
            );
            let absence = perry_mir::absence(ctx, ty).ok_or_else(|| {
                Diagnostic::internal("optional descriptor without absence states")
            })?;
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
        let layout = perry_mir::typed::<RecordType>(ctx, ty)
            .map(|record| self.state.record_layouts.get(ctx, record))
            .transpose()?;
        let count = layout.as_ref().map_or(0, |layout| layout.fields.len());
        let bytes = if layout.is_some() {
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
        let words = if let Some(layout) = layout {
            let tuple = layout.record.deref(ctx).is_tuple();
            let mut words = Vec::with_capacity(bytes / 4);
            words.extend([if tuple { JSON_TUPLE } else { RECORD }, count as u32]);
            let mut required = 0u32;
            let mut numeric = Vec::new();
            for (name, slot) in &layout.fields {
                let field_ty = slot.ty;
                let name_addr = self.state.data.string(name);
                let field = self.layout_descriptor(ctx, field_ty)?;
                words.extend([name_addr, field, slot.offset]);
                if !perry_mir::admits_undefined(ctx, field_ty)
                    && !field_ty.deref(ctx).is::<JsonType>()
                {
                    required += 1;
                }
                if let Some(number) = perry_abi::json::array_index(name.as_bytes()) {
                    numeric.push((number, slot.index as usize));
                }
            }
            let shape = self
                .state
                .static_table(ctx, StaticTable::PointerShape(layout.record, true))?;
            let table = self
                .state
                .static_table(ctx, StaticTable::FieldNames(layout.record))?;
            words.extend([layout.size, shape, table, required]);
            numeric.sort_unstable();
            let mut next = vec![u32::MAX; count];
            for (position, &(_, index)) in numeric.iter().enumerate() {
                next[index] = numeric
                    .get(position + 1)
                    .map_or(0, |&(_, index)| index as u32 + 1);
            }
            words.push(numeric.first().map_or(0, |&(_, index)| index as u32 + 1));
            for (name, next) in layout.fields.keys().zip(next) {
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
                ensure_internal!(
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
            let shape = self.state.data.pointer_shape(&[
                perry_abi::layout::variant::OWNER,
                perry_abi::layout::variant::JSON,
            ]);
            let table = self.state.data.field_names(["tag", "val"].into_iter());
            words.extend([shape, table, u32::from(self.state.retains_json)]);
            words
        } else if let Some(array) = perry_mir::typed::<ArrayType>(ctx, ty) {
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
            let kind = JsonSchema::scalar_descriptor(ctx, ty)
                .ok_or_else(|| unsupported!("unsupported native cause field type"))?;
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
