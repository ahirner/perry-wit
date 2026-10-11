//! Typed JSON serialization.
use crate::{
    function::FunctionLowering,
    repr::{i32_type, optional_inner, slot_kind, wasm_type},
    runtime::Runtime,
};
use anyhow::{Result, anyhow, ensure};
use perry_mir::{
    intrinsics::Intrinsic,
    types::{ArrayType, DateType, ExceptionType, JsonType, OptionalType, RecordType, StringType},
};
use perry_runtime::array as layout;
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

fn quote_json(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\0'..='\u{1f}' => out.push_str(&format!("\\u{:04x}", character as u32)),
            _ => out.push(character),
        }
    }
    out.push('"');
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
            // Finite numbers in ECMAScript notation (ryu-js); NaN and infinities as null.
            let size = self.i32_const(ctx, 40);
            let scratch = self.alloc(ctx, size);
            let difference = self.num(ctx, NumOpcode::F64Sub, vec![value, value]);
            let zero = self.f64_const(ctx, 0.0);
            let finite = self.num(ctx, NumOpcode::F64Eq, vec![difference, zero]);
            let number = self.new_block(ctx, vec![]);
            let null = self.new_block(ctx, vec![]);
            let join = self.new_block(ctx, vec![]);
            self.cond_br(ctx, finite, (number, vec![]), (null, vec![]));
            self.block = number;
            let len = self.runtime(ctx, Runtime::NumberFormat, vec![value, scratch]);
            self.runtime_void(ctx, Runtime::StdJsonRaw, vec![writer, scratch, len]);
            self.br(ctx, join, vec![]);
            self.block = null;
            self.json_raw(ctx, writer, "null");
            self.br(ctx, join, vec![]);
            self.block = join;
            return Ok(());
        }
        if deref.is::<StringType>() {
            drop(deref);
            self.runtime_void(ctx, Runtime::StdJsonQuote, vec![writer, value]);
            return Ok(());
        }
        if deref.is::<DateType>() {
            drop(deref);
            let text = self
                .date_intrinsic(ctx, Intrinsic::DateJson, vec![value])
                .expect("date JSON");
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
        let name: Identifier = format!("json_{}", self.state.json_serializers.len())
            .try_into()
            .expect("identifier");
        self.state.json_serializers.insert(ty, name.clone());
        let i32 = i32_type(ctx);
        let value_ty = wasm_type(ctx, ty)?;
        let fn_ty = FunctionType::get(ctx, vec![i32, value_ty], vec![]);
        let function = FuncOp::new(ctx, name.clone(), fn_ty);
        let entry = function.get_entry_block(ctx);
        let writer = entry.deref(ctx).get_argument(0);
        let value = entry.deref(ctx).get_argument(1);
        let mut b = FunctionLowering::glue(self.state, ctx, &function);
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx).ok();
        let tuple = record.is_some_and(|record| record.deref(ctx).is_tuple());
        let variant = TypedHandle::<perry_mir::types::VariantType>::from_handle(ty, ctx).ok();
        let (open, close) = if (record.is_some() && !tuple) || variant.is_some() {
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
                if record.is_some() {
                    perry_abi::layout::record::SOURCE
                } else if variant.is_some() {
                    perry_abi::layout::variant::SOURCE
                } else {
                    layout::SOURCE
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
        if let Some(record) = record {
            let fields: Vec<(String, TypeHandle)> = record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .iter()
                .map(|(n, t)| (n.to_string(), *t))
                .collect();
            let tuple_source = if tuple { source } else { None };
            let tuple_count = b.i32_const(ctx, fields.len() as i32);
            let tuple_length = tuple_source.map(|source| {
                b.runtime(
                    ctx,
                    Runtime::StdJsonSourceTupleLength,
                    vec![source, tuple_count],
                )
            });
            let ordered = if !tuple && b.state.json_provenance {
                let source = b.load(ctx, MemKind::I32, value, perry_abi::layout::record::SOURCE);
                let members = b.runtime(ctx, Runtime::StdJsonSourceBegin, vec![source]);
                let header = b.new_block(ctx, vec![i32]);
                let zero = b.i32_const(ctx, 0);
                b.br(ctx, header, vec![zero]);
                b.block = header;
                let index = header.deref(ctx).get_argument(0);
                let count = b.i32_const(ctx, fields.len() as i32);
                let field = b.runtime(
                    ctx,
                    Runtime::StdJsonSourceNext,
                    vec![members, writer, index, count],
                );
                Some((header, index, field, b.new_block(ctx, vec![])))
            } else {
                None
            };
            for (index, (field, field_ty)) in fields.into_iter().enumerate() {
                let dispatch_next = if let Some((_, _, actual, _)) = ordered {
                    let wanted = b.i32_const(ctx, index as i32);
                    let matches = b.num(ctx, NumOpcode::I32Eq, vec![actual, wanted]);
                    let selected = b.new_block(ctx, vec![]);
                    let next = b.new_block(ctx, vec![]);
                    b.cond_br(ctx, matches, (selected, vec![]), (next, vec![]));
                    b.block = selected;
                    Some(next)
                } else {
                    None
                };
                let kind = slot_kind(ctx, field_ty)?;
                let slot = b.load(ctx, kind, value, perry_abi::layout::record::field(index));
                if tuple {
                    let next = tuple_length.map(|length| {
                        let index = b.i32_const(ctx, index as i32);
                        let present = b.num(ctx, NumOpcode::I32LtU, vec![index, length]);
                        let write = b.new_block(ctx, vec![]);
                        let next = b.new_block(ctx, vec![]);
                        b.cond_br(ctx, present, (write, vec![]), (next, vec![]));
                        b.block = write;
                        next
                    });
                    b.runtime_void(ctx, Runtime::StdJsonItem, vec![writer]);
                    b.json_value(ctx, writer, slot, field_ty)?;
                    if let Some(next) = next {
                        b.br(ctx, next, vec![]);
                        b.block = next;
                    }
                    continue;
                }
                let mut key = String::new();
                quote_json(&mut key, &field);
                key.push(':');
                let key = b.state.data.string(&key);
                // Members holding `undefined` are left out.
                let present = match optional_inner(ctx, field_ty) {
                    Some(_) => {
                        let undefined = b.absence_test(ctx, slot, field_ty, false)?;
                        let present = b.num(ctx, NumOpcode::I32Eqz, vec![undefined]);
                        Some(present)
                    }
                    None => None,
                };
                let member = b.new_block(ctx, vec![]);
                let next = b.new_block(ctx, vec![]);
                match present {
                    Some(present) => b.cond_br(ctx, present, (member, vec![]), (next, vec![])),
                    None => b.br(ctx, member, vec![]),
                }
                b.block = member;
                let key = b.i32_const(ctx, key as i32);
                b.runtime_void(ctx, Runtime::StdJsonKey, vec![writer, key]);
                b.json_value(ctx, writer, slot, field_ty)?;
                b.br(ctx, next, vec![]);
                b.block = next;
                if let Some((header, index, _, _)) = ordered {
                    let one = b.i32_const(ctx, 1);
                    let advanced = b.num(ctx, NumOpcode::I32Add, vec![index, one]);
                    b.br(ctx, header, vec![advanced]);
                    b.block = dispatch_next.expect("ordered dispatch");
                }
            }
            if let Some((_, _, _, finished)) = ordered {
                b.br(ctx, finished, vec![]);
                b.block = finished;
            }
            if let Some(source) = tuple_source {
                b.runtime_void(
                    ctx,
                    Runtime::StdJsonSourceTupleTail,
                    vec![source, writer, tuple_count],
                );
            }
        } else if let Some(variant) = variant {
            b.json_variant(ctx, writer, value, variant)?;
        } else {
            let element = FunctionLowering::element_type(ctx, value).or_else(|_| {
                TypedHandle::<ArrayType>::from_handle(ty, ctx)
                    .map(|a| a.deref(ctx).element())
                    .map_err(|_| anyhow!("array"))
            })?;
            let kind = slot_kind(ctx, element)?;
            let len = b.load(ctx, MemKind::I32, value, layout::LEN);
            let data = b.load(ctx, MemKind::I32, value, layout::DATA);
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
