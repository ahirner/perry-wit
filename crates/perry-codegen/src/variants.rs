//! Compact finite variants: tag string, traced payload owner, and one eight-byte value slot.
use crate::{FunctionLowering, Repr, Runtime, repr, slot_kind};
use anyhow::Result;
use perry_abi::layout::variant;
use perry_mir::{types::VariantType, variants::admits_payload};
use pliron::{
    context::Context,
    r#type::{TypeHandle, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_, '_> {
    pub(super) fn number_string_equal(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        a: Value,
        b: Value,
    ) -> Result<Value> {
        let result_type = crate::i32_type(ctx);
        let done = self.new_block(ctx, vec![result_type]);
        if crate::repr::optional_inner(ctx, ty).is_some() {
            let absent_a = self.is_absent(ctx, a, ty)?;
            let absent_b = self.is_absent(ctx, b, ty)?;
            let either = self.num(ctx, NumOpcode::I32Or, vec![absent_a, absent_b]);
            let same = self.num(ctx, NumOpcode::I32Eq, vec![a, b]);
            let present = self.new_block(ctx, vec![]);
            self.cond_br(ctx, either, (done, vec![same]), (present, vec![]));
            self.block = present;
        }
        let tag_a = self.load(ctx, MemKind::I32, a, variant::TAG);
        let tag_b = self.load(ctx, MemKind::I32, b, variant::TAG);
        let same_tag = self.num(ctx, NumOpcode::I32Eq, vec![tag_a, tag_b]);
        let same = self.new_block(ctx, vec![]);
        let no = self.i32_const(ctx, 0);
        self.cond_br(ctx, same_tag, (same, vec![]), (done, vec![no]));
        self.block = same;
        let number = self.state.data.string("number");
        let number = self.i32_const(ctx, number as i32);
        let is_number = self.num(ctx, NumOpcode::I32Eq, vec![tag_a, number]);
        let numbers = self.new_block(ctx, vec![]);
        let strings = self.new_block(ctx, vec![]);
        self.cond_br(ctx, is_number, (numbers, vec![]), (strings, vec![]));
        self.block = numbers;
        let a_number = self.load(ctx, MemKind::F64, a, variant::VALUE);
        let b_number = self.load(ctx, MemKind::F64, b, variant::VALUE);
        let equal = self.num(ctx, NumOpcode::F64Eq, vec![a_number, b_number]);
        self.br(ctx, done, vec![equal]);
        self.block = strings;
        let a_string = self.load(ctx, MemKind::I32, a, variant::VALUE);
        let b_string = self.load(ctx, MemKind::I32, b, variant::VALUE);
        let order = self.runtime(ctx, Runtime::StringCompare, vec![a_string, b_string]);
        let equal = self.num(ctx, NumOpcode::I32Eqz, vec![order]);
        self.br(ctx, done, vec![equal]);
        self.block = done;
        Ok(done.deref(ctx).get_argument(0))
    }

    pub(super) fn json_number_string(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
    ) -> Result<()> {
        let tag = self.load(ctx, MemKind::I32, value, variant::TAG);
        let number = self.state.data.string("number");
        let number = self.i32_const(ctx, number as i32);
        let is_number = self.num(ctx, NumOpcode::I32Eq, vec![tag, number]);
        let numbers = self.new_block(ctx, vec![]);
        let strings = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.cond_br(ctx, is_number, (numbers, vec![]), (strings, vec![]));
        self.block = numbers;
        let payload = self.load(ctx, MemKind::F64, value, variant::VALUE);
        let ty = pliron::builtin::types::FP64Type::get(ctx).into();
        self.json_value(ctx, writer, payload, ty)?;
        self.br(ctx, done, vec![]);
        self.block = strings;
        let payload = self.load(ctx, MemKind::I32, value, variant::VALUE);
        let ty = perry_mir::types::StringType::get(ctx).into();
        self.json_value(ctx, writer, payload, ty)?;
        self.br(ctx, done, vec![]);
        self.block = done;
        Ok(())
    }

    pub(super) fn variant_new(
        &mut self,
        ctx: &mut Context,
        tag: &str,
        payload: Option<(TypeHandle, Value)>,
    ) -> Result<Value> {
        let size = self.i32_const(
            ctx,
            if self.state.json_provenance {
                perry_abi::layout::variant::SIZE as i32
            } else {
                variant::BASE_SIZE as i32
            },
        );
        let object = if self.state.collectible {
            let shape = self.pointer_shape(if self.state.json_provenance {
                &[variant::OWNER, variant::SOURCE]
            } else {
                &[variant::OWNER]
            });
            let shape = self.i32_const(ctx, shape as i32);
            self.runtime(ctx, Runtime::GcAlloc, vec![size, shape])
        } else {
            self.alloc(ctx, size)
        };
        let text = self.state.data.string(tag);
        let text = self.i32_const(ctx, text as i32);
        self.store(ctx, MemKind::I32, object, variant::TAG, text);
        let owner = match payload {
            Some((ty, value)) if repr(ctx, ty)? == Repr::Ref => value,
            _ => self.i32_const(ctx, 0),
        };
        self.store(ctx, MemKind::I32, object, variant::OWNER, owner);
        if self.state.json_provenance {
            let zero = self.i32_const(ctx, 0);
            self.store(
                ctx,
                MemKind::I32,
                object,
                perry_abi::layout::variant::SOURCE,
                zero,
            );
        }
        if let Some((ty, value)) = payload {
            self.store(ctx, slot_kind(ctx, ty)?, object, variant::VALUE, value);
        }
        Ok(object)
    }

    pub(super) fn json_variant(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        variant: TypedHandle<VariantType>,
    ) -> Result<()> {
        let cases = variant.deref(ctx).cases().to_vec();
        let tag = self.load(ctx, MemKind::I32, value, variant::TAG);
        let ordered = if self.state.json_provenance {
            let source = self.load(ctx, MemKind::I32, value, perry_abi::layout::variant::SOURCE);
            let members = self.runtime(ctx, Runtime::StdJsonSourceBegin, vec![source]);
            let i32 = crate::i32_type(ctx);
            let header = self.new_block(ctx, vec![i32]);
            let advance = self.new_block(ctx, vec![]);
            let finished = self.new_block(ctx, vec![]);
            let zero = self.i32_const(ctx, 0);
            self.br(ctx, header, vec![zero]);
            self.block = header;
            let index = header.deref(ctx).get_argument(0);
            let count = self.i32_const(ctx, 2);
            let field = self.runtime(
                ctx,
                Runtime::StdJsonSourceNext,
                vec![members, writer, index, count],
            );
            let tag_case = self.new_block(ctx, vec![]);
            let rest = self.new_block(ctx, vec![]);
            let is_tag = self.num(ctx, NumOpcode::I32Eqz, vec![field]);
            self.cond_br(ctx, is_tag, (tag_case, vec![]), (rest, vec![]));
            self.block = tag_case;
            Some((header, index, field, advance, finished, rest))
        } else {
            None
        };
        let key = self.state.data.string("\"tag\":");
        let key = self.i32_const(ctx, key as i32);
        self.runtime_void(ctx, Runtime::StdJsonKey, vec![writer, key]);
        self.runtime_void(ctx, Runtime::StdJsonQuote, vec![writer, tag]);
        if let Some((_, _, field, advance, finished, rest)) = ordered {
            self.br(ctx, advance, vec![]);
            self.block = rest;
            let one = self.i32_const(ctx, 1);
            let is_payload = self.num(ctx, NumOpcode::I32Eq, vec![field, one]);
            let payload = self.new_block(ctx, vec![]);
            self.cond_br(ctx, is_payload, (payload, vec![]), (finished, vec![]));
            self.block = payload;
        }
        let done = self.new_block(ctx, vec![]);
        for (name, payload) in cases {
            let Some(payload) = payload else {
                continue;
            };
            let text = self.state.data.string(&name);
            let text = self.i32_const(ctx, text as i32);
            let comparison = self.runtime(ctx, Runtime::StringCompare, vec![tag, text]);
            let matches = self.num(ctx, NumOpcode::I32Eqz, vec![comparison]);
            let member = self.new_block(ctx, vec![]);
            let next = self.new_block(ctx, vec![]);
            self.cond_br(ctx, matches, (member, vec![]), (next, vec![]));
            self.block = member;
            let slot = self.load(ctx, slot_kind(ctx, payload)?, value, variant::VALUE);
            if perry_mir::optional::absence(ctx, payload).is_some_and(|a| a.admits_undefined()) {
                let absent = self.absence_test(ctx, slot, payload, false)?;
                let present = self.new_block(ctx, vec![]);
                self.cond_br(ctx, absent, (done, vec![]), (present, vec![]));
                self.block = present;
            }
            let key = self.state.data.string("\"val\":");
            let key = self.i32_const(ctx, key as i32);
            self.runtime_void(ctx, Runtime::StdJsonKey, vec![writer, key]);
            self.json_value(ctx, writer, slot, payload)?;
            self.br(ctx, done, vec![]);
            self.block = next;
        }
        self.br(ctx, done, vec![]);
        self.block = done;
        if let Some((header, index, _, advance, finished, _)) = ordered {
            self.br(ctx, advance, vec![]);
            self.block = advance;
            let one = self.i32_const(ctx, 1);
            let index = self.num(ctx, NumOpcode::I32Add, vec![index, one]);
            self.br(ctx, header, vec![index]);
            self.block = finished;
        }
        Ok(())
    }

    pub(super) fn variant_payload(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        object: Value,
        result: TypeHandle,
    ) -> Result<Value> {
        let cases = perry_mir::variants::cases(ctx, ty)?;
        let tag = self.load(ctx, MemKind::I32, object, variant::TAG);
        let mut admitted = self.i32_const(ctx, 0);
        let mut present = self.i32_const(ctx, 0);
        for (name, payload) in cases {
            if !admits_payload(ctx, result, payload) {
                continue;
            }
            let text = self.state.data.string(&name);
            let text = self.i32_const(ctx, text as i32);
            let comparison = self.runtime(ctx, Runtime::StringCompare, vec![tag, text]);
            let matches = self.num(ctx, NumOpcode::I32Eqz, vec![comparison]);
            admitted = self.num(ctx, NumOpcode::I32Or, vec![admitted, matches]);
            if payload.is_some() {
                present = self.num(ctx, NumOpcode::I32Or, vec![present, matches]);
            }
        }
        let invalid = self.num(ctx, NumOpcode::I32Eqz, vec![admitted]);
        self.throw_if(ctx, invalid, perry_mir::attrs::ErrorKind::TypeError);
        let kind = slot_kind(ctx, result)?;
        let value = self.load(ctx, kind, object, variant::VALUE);
        if perry_mir::optional::absence(ctx, result).is_some_and(|a| a.admits_undefined()) {
            let undefined = self.undefined(ctx, result)?;
            Ok(self.select(ctx, present, value, undefined))
        } else {
            Ok(value)
        }
    }
}
