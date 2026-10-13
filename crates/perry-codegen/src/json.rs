//! Typed JSON serialization.
use crate::{
    function::FunctionLowering,
    repr::{i32_type, optional_inner, slot_kind, wasm_type},
    runtime::Runtime,
};
use anyhow::{Result, anyhow, ensure};
use perry_abi::layout::{array, record};
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
    pub(super) fn json_cursor_key(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        ty: TypeHandle,
    ) -> Result<()> {
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx)?;
        let mut names = record
            .deref(ctx)
            .fields()?
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.to_string(), i as u32))
            .collect::<Vec<_>>();
        names.sort_unstable_by(|a, b| {
            a.0.len()
                .cmp(&b.0.len())
                .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
        });
        let mut bytes = Vec::with_capacity(names.len() * 8);
        for (name, index) in &names {
            bytes.extend_from_slice(&self.state.data.string(name).to_le_bytes());
            bytes.extend_from_slice(&index.to_le_bytes());
        }
        let table = self.state.data.append(4, &bytes);
        let table = self.i32_const(ctx, table as i32);
        let count = self.i32_const(ctx, names.len() as i32);
        let cursor = self.operand(ctx, op, 0)?;
        let index = self.runtime(ctx, Runtime::JsonCursorKey, vec![cursor, table, count]);
        self.define(ctx, op, index);
        Ok(())
    }

    pub(crate) fn native_descriptor(&mut self, ctx: &mut Context, ty: TypeHandle) -> Result<u32> {
        if let Some(&address) = self.state.native_descriptors.get(&ty) {
            return Ok(address);
        }
        let record_fields = TypedHandle::<RecordType>::from_handle(ty, ctx)
            .ok()
            .map(|record| record.deref(ctx).fields().map(|fields| fields.to_vec()))
            .transpose()
            .map_err(|_| anyhow!("unresolved native record"))?;
        let count = record_fields.as_ref().map_or(0, Vec::len);
        let address = self.state.data.append(4, &vec![0; 8 + 12 * count]);
        self.state.native_descriptors.insert(ty, address);
        let mut words = if let Some(fields) = record_fields {
            let mut words = vec![4, count as u32];
            for (index, (name, field)) in fields.into_iter().enumerate() {
                let name = self.state.data.string(&name);
                let field = self.native_descriptor(ctx, field)?;
                words.extend([name, field, record::field(index)]);
            }
            words
        } else if let Some(inner) = optional_inner(ctx, ty) {
            let inner = self.native_descriptor(ctx, inner)?;
            vec![7, inner]
        } else if let Ok(array) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            let element = array.deref(ctx).element();
            let element = self.native_descriptor(ctx, element)?;
            vec![5, element]
        } else {
            let kind = if ty.deref(ctx).is::<FP64Type>() {
                1
            } else if ty.deref(ctx).is::<IntegerType>() {
                2
            } else if ty.deref(ctx).is::<StringType>() {
                3
            } else if ty.deref(ctx).is::<ExceptionType>() {
                6
            } else {
                return Err(anyhow!("unsupported native cause field type"));
            };
            vec![kind, 0]
        };
        let offset = (address - crate::DATA_BASE) as usize;
        for (index, word) in words.drain(..).enumerate() {
            self.state.data.bytes[offset + index * 4..offset + index * 4 + 4]
                .copy_from_slice(&word.to_le_bytes());
        }
        Ok(address)
    }

    fn static_text(&mut self, ctx: &mut Context, text: &str) -> (Value, Value) {
        let address = self.state.data.string(text) + perry_abi::layout::string::SIZE;
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
        let name = crate::host::symbol(pliron::ident!("json_"), &index);
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
                    fields.iter().map(|(name, _)| name.to_string()).collect()
                };
                (fields.iter().map(|&(_, ty)| ty).collect(), keys)
            };
            let count = b.i32_const(ctx, types.len() as i32);
            if tuple {
                b.json_tuple_items(ctx, writer, value, &types)?;
            } else {
                b.json_record_members(ctx, writer, value, &keys, &types)?;
            }
            // A tuple has no keys: its extras are the elements past `count`.
            b.json_extras(ctx, writer, value, record::JSON, &keys, count);
        } else if let Some(variant_type) = variant_type {
            let count = b.json_variant(ctx, writer, value, variant_type)?;
            b.json_extras(
                ctx,
                writer,
                value,
                perry_abi::layout::variant::JSON,
                &["tag", "val"],
                count,
            );
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
        b.br(ctx, done, vec![]);
        b.block = done;
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, function.get_operation(), 0);
        Ok(name)
    }

    /// Retained graph values fill only fields outside the declared shape.
    fn json_extras(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        offset: u32,
        fields: &[impl AsRef<str>],
        count: Value,
    ) {
        if !self.state.retains_json {
            return;
        }
        // `std_json_extras` binary-searches the names by byte length, then bytes.
        let mut fields: Vec<&str> = fields.iter().map(AsRef::as_ref).collect();
        fields.sort_unstable_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        let addresses: Vec<_> = fields
            .iter()
            .flat_map(|name| self.state.data.string(name).to_le_bytes())
            .collect();
        let table = self.state.data.append(4, &addresses);
        let table = self.i32_const(ctx, table as i32);
        let graph = self.load(ctx, MemKind::I32, value, offset);
        self.runtime_void(
            ctx,
            Runtime::StdJsonExtras,
            vec![writer, graph, table, count],
        );
    }

    /// Serialize the declared tuple items.
    fn json_tuple_items(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        items: &[TypeHandle],
    ) -> Result<()> {
        for (index, &field_ty) in items.iter().enumerate() {
            let kind = slot_kind(ctx, field_ty)?;
            let slot = self.load(ctx, kind, value, record::field(index));
            self.runtime_void(ctx, Runtime::StdJsonItem, vec![writer]);
            self.json_value(ctx, writer, slot, field_ty)?;
        }
        Ok(())
    }

    /// Serialize the declared record fields, omitting `undefined`.
    fn json_record_members(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        keys: &[String],
        types: &[TypeHandle],
    ) -> Result<()> {
        for (index, (key, &field_ty)) in keys.iter().zip(types).enumerate() {
            let kind = slot_kind(ctx, field_ty)?;
            let slot = self.load(ctx, kind, value, record::field(index));
            let key = self.state.data.string(&json_key(key));
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
