//! Typed JSON serialization.
use crate::{
    function::FunctionLowering,
    repr::{i32_type, optional_inner, slot_kind, wasm_type},
    runtime::Runtime,
};
use anyhow::{Result, anyhow, ensure};
use perry_abi::layout::{array, record, variant};
use perry_mir::{
    intrinsics::Intrinsic,
    types::{ArrayType, DateType, ExceptionType, JsonType, OptionalType, RecordType, StringType},
};
use pliron::{
    builtin::{
        op_interfaces::SingleBlockRegionInterface,
        ops::FuncOp,
        types::{FP64Type, FunctionType, IntegerType},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    op::Op,
    operation::Operation,
    printable::Printable,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::ReturnOp,
    ops::CallOp,
};
use std::fmt::Write as _;

/// The member prefix `"name":` of a record field known at compile time, quoted as ECMAScript
/// `QuoteJSONString` does.
fn json_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 3);
    out.push('"');
    for character in name.chars() {
        match character {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            // Writing to a `String` cannot fail.
            '\0'..='\u{1f}' => _ = write!(out, "\\u{:04x}", character as u32),
            _ => out.push(character),
        }
    }
    out.push_str("\":");
    out
}

impl FunctionLowering<'_, '_> {
    fn static_text(&mut self, ctx: &mut Context, text: &str) -> (Value, Value) {
        let address = self.state.data.string(text) + perry_runtime::string::SIZE;
        (
            self.i32_const(ctx, address as i32),
            self.i32_const(ctx, text.len() as i32),
        )
    }

    fn json_raw(&mut self, ctx: &mut Context, writer: Value, text: &str) {
        let (data, len) = self.static_text(ctx, text);
        self.runtime_void(ctx, Runtime::StdJsonRaw, vec![writer, data, len]);
    }

    pub(super) fn json_value(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        ty: TypeHandle,
    ) -> Result<()> {
        if let Some(inner) = optional_inner(ctx, ty) {
            // Inside arrays, `undefined` is written as `null`.
            let undefined = self.is_absent(ctx, value, ty)?;
            let null = self.new_block(ctx, vec![]);
            let defined = self.new_block(ctx, vec![]);
            let join = self.new_block(ctx, vec![]);
            self.cond_br(ctx, undefined, (null, vec![]), (defined, vec![]));
            self.block = null;
            self.json_raw(ctx, writer, "null");
            self.br(ctx, join, vec![]);
            self.block = defined;
            self.json_value(ctx, writer, value, inner)?;
            self.br(ctx, join, vec![]);
            self.block = join;
            return Ok(());
        }
        if ty.deref(ctx).is::<perry_mir::types::NumberStringType>() {
            return self.json_number_string(ctx, writer, value);
        }
        let deref = ty.deref(ctx);
        if deref.is::<FP64Type>() {
            drop(deref);
            self.runtime_void(ctx, Runtime::StdJsonNumber, vec![writer, value]);
            return Ok(());
        }
        if deref.is::<StringType>() {
            drop(deref);
            self.runtime_void(ctx, Runtime::StdJsonQuote, vec![writer, value]);
            return Ok(());
        }
        if deref.is::<DateType>() {
            drop(deref);
            let text = self.date_intrinsic(ctx, Intrinsic::DateJson, vec![value])?;
            let string = StringType::get(ctx).into();
            let nullable =
                OptionalType::get_with(ctx, string, perry_mir::attrs::Absence::Null).into();
            return self.json_value(ctx, writer, text, nullable);
        }
        if deref.is::<ExceptionType>() {
            drop(deref);
            self.json_raw(ctx, writer, "{}");
            return Ok(());
        }
        if deref.is::<JsonType>() {
            drop(deref);
            self.runtime_void(ctx, Runtime::StdJsonValue, vec![writer, value]);
            return Ok(());
        }
        if deref
            .downcast_ref::<IntegerType>()
            .is_some_and(|i| i.width() == 1)
        {
            drop(deref);
            let (true_data, true_len) = self.static_text(ctx, "true");
            let (false_data, false_len) = self.static_text(ctx, "false");
            let data = self.select(ctx, value, true_data, false_data);
            let len = self.select(ctx, value, true_len, false_len);
            self.runtime_void(ctx, Runtime::StdJsonRaw, vec![writer, data, len]);
            return Ok(());
        }
        let container = deref.is::<RecordType>()
            || deref.is::<ArrayType>()
            || deref.is::<perry_mir::types::VariantType>();
        drop(deref);
        ensure!(
            container,
            "JSON.stringify of {} is not supported yet",
            ty.disp(ctx)
        );
        let serializer = self.json_serializer(ctx, ty)?;
        let call = CallOp::new(ctx, serializer, vec![writer, value], vec![]).get_operation();
        self.insert(ctx, call);
        Ok(())
    }

    fn json_serializer(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<Identifier> {
        if let Some(name) = self.state.json_serializers.get(&ty) {
            return Ok(name.clone());
        }
        let index = self.state.json_serializers.len().to_string();
        let name = crate::host::symbol("json_", &index);
        self.state.json_serializers.insert(ty, name.clone());
        let i32 = i32_type(ctx);
        let value_ty = wasm_type(ctx, ty)?;
        let fn_ty = FunctionType::get(ctx, vec![i32, value_ty], vec![]);
        let function = FuncOp::new(ctx, name.clone(), fn_ty);
        let entry = function.get_entry_block(ctx);
        let writer = entry.deref(ctx).get_argument(0);
        let value = entry.deref(ctx).get_argument(1);
        let mut b = FunctionLowering::glue(self.state, ctx, &function);
        let record_type = TypedHandle::<RecordType>::from_handle(ty, ctx).ok();
        let tuple = record_type.is_some_and(|record| record.deref(ctx).is_tuple());
        let variant_type = TypedHandle::<perry_mir::types::VariantType>::from_handle(ty, ctx).ok();
        let (open, close) = if (record_type.is_some() && !tuple) || variant_type.is_some() {
            ('{', '}')
        } else {
            ('[', ']')
        };
        let done = b.new_block(ctx, vec![]);
        let source = if b.state.json_provenance {
            let source = b.load(
                ctx,
                MemKind::I32,
                value,
                if record_type.is_some() {
                    record::SOURCE
                } else if variant_type.is_some() {
                    variant::SOURCE
                } else {
                    array::SOURCE
                },
            );
            let copied = b.runtime(ctx, Runtime::StdJsonSourceCopy, vec![source, writer]);
            let write = b.new_block(ctx, vec![]);
            b.cond_br(ctx, copied, (done, vec![]), (write, vec![]));
            b.block = write;
            Some(source)
        } else {
            None
        };
        let bracket = b.i32_const(ctx, open as i32);
        let opened = b.runtime(ctx, Runtime::StdJsonOpen, vec![writer, bracket]);
        let body = b.new_block(ctx, vec![]);
        b.cond_br(ctx, opened, (body, vec![]), (done, vec![]));
        b.block = body;
        if let Some(record_type) = record_type {
            // Detached from the context, which the writers below mutate.
            let (types, keys): (Vec<TypeHandle>, Vec<String>) = {
                let record_type = record_type.deref(ctx);
                let fields = record_type
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?;
                let keys = if tuple {
                    Vec::new()
                } else {
                    fields.iter().map(|(name, _)| json_key(name)).collect()
                };
                (fields.iter().map(|&(_, ty)| ty).collect(), keys)
            };
            if tuple {
                b.json_tuple_items(ctx, writer, value, &types, source)?;
            } else {
                b.json_record_members(ctx, writer, value, &keys, &types, source)?;
            }
        } else if let Some(variant_type) = variant_type {
            b.json_variant(ctx, writer, value, variant_type)?;
        } else {
            let element = FunctionLowering::element_type(ctx, value).or_else(|_| {
                TypedHandle::<ArrayType>::from_handle(ty, ctx)
                    .map(|a| a.deref(ctx).element())
                    .map_err(|_| anyhow!("array"))
            })?;
            let kind = slot_kind(ctx, element)?;
            let len = b.load(ctx, MemKind::I32, value, array::LEN);
            let data = b.load(ctx, MemKind::I32, value, array::DATA);
            b.counted_loop(ctx, len, |b, ctx, i| {
                b.runtime_void(ctx, Runtime::StdJsonItem, vec![writer]);
                let slot = b.slot_address(ctx, data, i);
                let item = b.load(ctx, kind, slot, 0);
                b.json_value(ctx, writer, item, element)
            })?;
        }
        let bracket = b.i32_const(ctx, close as i32);
        b.runtime_void(ctx, Runtime::StdJsonClose, vec![writer, bracket]);
        if let Some(source) = source {
            b.runtime_void(ctx, Runtime::StdJsonSourceRemember, vec![source, writer]);
        }
        b.br(ctx, done, vec![]);
        b.block = done;
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, function.get_operation(), 0);
        Ok(name)
    }

    /// Items of tuple `value`. With a parse `source`, only the leading items the parsed array
    /// had, then its items beyond the declared ones.
    fn json_tuple_items(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        items: &[TypeHandle],
        source: Option<Value>,
    ) -> Result<()> {
        let count = self.i32_const(ctx, items.len() as i32);
        let length = source.map(|source| {
            self.runtime(ctx, Runtime::StdJsonSourceTupleLength, vec![source, count])
        });
        for (index, &field_ty) in items.iter().enumerate() {
            let kind = slot_kind(ctx, field_ty)?;
            let slot = self.load(ctx, kind, value, record::field(index));
            let next = length.map(|length| {
                let index = self.i32_const(ctx, index as i32);
                let present = self.num(ctx, NumOpcode::I32LtU, vec![index, length]);
                let write = self.new_block(ctx, vec![]);
                let next = self.new_block(ctx, vec![]);
                self.cond_br(ctx, present, (write, vec![]), (next, vec![]));
                self.block = write;
                next
            });
            self.runtime_void(ctx, Runtime::StdJsonItem, vec![writer]);
            self.json_value(ctx, writer, slot, field_ty)?;
            if let Some(next) = next {
                self.br(ctx, next, vec![]);
                self.block = next;
            }
        }
        if let Some(source) = source {
            self.runtime_void(
                ctx,
                Runtime::StdJsonSourceTupleTail,
                vec![source, writer, count],
            );
        }
        Ok(())
    }

    /// Members of record `value` under their `"name":` `keys`, leaving out `undefined` ones.
    /// With a parse `source`, a loop asks the runtime for the next field in the parsed member
    /// order (copying members the record does not declare) and dispatches to that field's
    /// writer.
    fn json_record_members(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        keys: &[String],
        types: &[TypeHandle],
        source: Option<Value>,
    ) -> Result<()> {
        let ordered = source.map(|source| {
            let members = self.runtime(ctx, Runtime::StdJsonSourceBegin, vec![source]);
            let i32 = i32_type(ctx);
            let header = self.new_block(ctx, vec![i32]);
            let zero = self.i32_const(ctx, 0);
            self.br(ctx, header, vec![zero]);
            self.block = header;
            let index = header.deref(ctx).get_argument(0);
            let count = self.i32_const(ctx, types.len() as i32);
            let field = self.runtime(
                ctx,
                Runtime::StdJsonSourceNext,
                vec![members, writer, index, count],
            );
            let finished = self.new_block(ctx, vec![]);
            (header, index, field, finished)
        });
        for (index, (key, &field_ty)) in keys.iter().zip(types).enumerate() {
            // With a source, this member's writer runs only when the runtime selects it.
            let skip = ordered.map(|(_, _, selected, _)| {
                let wanted = self.i32_const(ctx, index as i32);
                let matches = self.num(ctx, NumOpcode::I32Eq, vec![selected, wanted]);
                let write = self.new_block(ctx, vec![]);
                let skip = self.new_block(ctx, vec![]);
                self.cond_br(ctx, matches, (write, vec![]), (skip, vec![]));
                self.block = write;
                skip
            });
            let kind = slot_kind(ctx, field_ty)?;
            let slot = self.load(ctx, kind, value, record::field(index));
            let key = self.state.data.string(key);
            let present = match optional_inner(ctx, field_ty) {
                Some(_) => {
                    let undefined = self.absence_test(ctx, slot, field_ty, false)?;
                    Some(self.num(ctx, NumOpcode::I32Eqz, vec![undefined]))
                }
                None => None,
            };
            let member = self.new_block(ctx, vec![]);
            let next = self.new_block(ctx, vec![]);
            match present {
                Some(present) => self.cond_br(ctx, present, (member, vec![]), (next, vec![])),
                None => self.br(ctx, member, vec![]),
            }
            self.block = member;
            let key = self.i32_const(ctx, key as i32);
            self.runtime_void(ctx, Runtime::StdJsonKey, vec![writer, key]);
            self.json_value(ctx, writer, slot, field_ty)?;
            self.br(ctx, next, vec![]);
            self.block = next;
            if let Some(((header, index, _, _), skip)) = ordered.zip(skip) {
                let one = self.i32_const(ctx, 1);
                let advanced = self.num(ctx, NumOpcode::I32Add, vec![index, one]);
                self.br(ctx, header, vec![advanced]);
                self.block = skip;
            }
        }
        if let Some((_, _, _, finished)) = ordered {
            self.br(ctx, finished, vec![]);
            self.block = finished;
        }
        Ok(())
    }

    pub(super) fn json_stringify(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        let value = self.operand(ctx, op, 0)?;
        let ty = op.deref(ctx).get_operand(0).get_type(ctx);
        let writer = self.runtime(ctx, Runtime::StdJsonWriter, vec![]);
        let json = ty.deref(ctx).is::<JsonType>();
        let text = if json || optional_inner(ctx, ty).is_some() {
            // A top-level `undefined` stringifies to `undefined`.
            let undefined = if json {
                self.num(ctx, NumOpcode::I32Eqz, vec![value])
            } else {
                self.absence_test(ctx, value, ty, false)?
            };
            let i32 = i32_type(ctx);
            let write = self.new_block(ctx, vec![]);
            let join = self.new_block(ctx, vec![i32]);
            let null = self.i32_const(ctx, 0);
            self.cond_br(ctx, undefined, (join, vec![null]), (write, vec![]));
            self.block = write;
            self.json_value(ctx, writer, value, ty)?;
            let text = self.json_finish(ctx, writer);
            self.br(ctx, join, vec![text]);
            self.block = join;
            join.deref(ctx).get_argument(0)
        } else {
            self.json_value(ctx, writer, value, ty)?;
            self.json_finish(ctx, writer)
        };
        self.define(ctx, op, text);
        Ok(())
    }

    fn json_finish(&mut self, ctx: &mut Context, writer: Value) -> Value {
        let text = self.runtime(ctx, Runtime::StdJsonFinish, vec![writer]);
        let status = self.runtime(ctx, Runtime::Status, vec![]);
        self.throw_status_if(ctx, status, status);
        text
    }
}
