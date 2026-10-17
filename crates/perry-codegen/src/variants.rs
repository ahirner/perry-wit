//! Compact finite variants: tag string, traced payload owner, and one eight-byte value slot.
use crate::{
    function::FunctionLowering,
    repr::{Repr, i32_type, optional_inner, repr, slot_kind},
    runtime::Runtime,
};
use anyhow::Result;
use perry_abi::layout::variant;
use perry_mir::{VariantType, admits_payload};
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
        let result_type = i32_type(ctx);
        let done = self.new_block(ctx, vec![result_type]);
        if optional_inner(ctx, ty).is_some() {
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
        let ty = perry_mir::StringType::get(ctx).into();
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
            (variant::SIZE + u32::from(self.state.retains_json) * 8) as i32,
        );
        let object = if self.state.collectible {
            let pointers = [variant::OWNER, variant::JSON];
            let shape = self.pointer_shape(&pointers[..1 + usize::from(self.state.retains_json)]);
            let shape = self.i32_const(ctx, shape as i32);
            self.runtime(ctx, Runtime::GcAlloc, vec![size, shape])
        } else {
            self.alloc(ctx, size)
        };
        if self.state.retains_json {
            let zero = self.i32_const(ctx, 0);
            self.store(ctx, MemKind::I32, object, variant::JSON, zero);
        }
        let text = self.state.data.string(tag);
        let text = self.i32_const(ctx, text as i32);
        self.store(ctx, MemKind::I32, object, variant::TAG, text);
        let owner = match payload {
            Some((ty, value)) if repr(ctx, ty)? == Repr::Ref => value,
            _ => self.i32_const(ctx, 0),
        };
        self.store(ctx, MemKind::I32, object, variant::OWNER, owner);
        if let Some((ty, value)) = payload {
            self.store(ctx, slot_kind(ctx, ty)?, object, variant::VALUE, value);
        }
        Ok(object)
    }

    /// Serialize the active variant and return its declared member count.
    pub(super) fn json_variant(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        ty: TypedHandle<VariantType>,
    ) -> Result<Value> {
        let payload_cases: Vec<(String, TypeHandle)> = ty
            .deref(ctx)
            .cases()
            .iter()
            .filter_map(|(name, payload)| payload.map(|payload| (name.clone(), payload)))
            .collect();
        let tag = self.load(ctx, MemKind::I32, value, variant::TAG);
        let key = self.state.data.string("\"tag\":");
        let key = self.i32_const(ctx, key as i32);
        self.runtime_void(ctx, Runtime::StdJsonKey, vec![writer, key]);
        self.runtime_void(ctx, Runtime::StdJsonQuote, vec![writer, tag]);
        let count_ty = i32_type(ctx);
        let done = self.new_block(ctx, vec![count_ty]);
        let two = self.i32_const(ctx, 2);
        for (name, payload) in payload_cases {
            let text = self.state.data.string(&name);
            let text = self.i32_const(ctx, text as i32);
            let comparison = self.runtime(ctx, Runtime::StringCompare, vec![tag, text]);
            let matches = self.num(ctx, NumOpcode::I32Eqz, vec![comparison]);
            let member = self.new_block(ctx, vec![]);
            let next = self.new_block(ctx, vec![]);
            self.cond_br(ctx, matches, (member, vec![]), (next, vec![]));
            self.block = member;
            let slot = self.load(ctx, slot_kind(ctx, payload)?, value, variant::VALUE);
            if perry_mir::absence(ctx, payload).is_some_and(|a| a.admits_undefined()) {
                let absent = self.absence_test(ctx, slot, payload, false)?;
                let present = self.new_block(ctx, vec![]);
                self.cond_br(ctx, absent, (done, vec![two]), (present, vec![]));
                self.block = present;
            }
            let key = self.state.data.string("\"val\":");
            let key = self.i32_const(ctx, key as i32);
            self.runtime_void(ctx, Runtime::StdJsonKey, vec![writer, key]);
            self.json_value(ctx, writer, slot, payload)?;
            self.br(ctx, done, vec![two]);
            self.block = next;
        }
        let one = self.i32_const(ctx, 1);
        self.br(ctx, done, vec![one]);
        self.block = done;
        Ok(done.deref(ctx).get_argument(0))
    }

    pub(super) fn variant_payload(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        object: Value,
        result: TypeHandle,
    ) -> Result<Value> {
        let cases = perry_mir::variant_cases(ctx, ty)?;
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
        self.throw_if(ctx, invalid, perry_mir::ErrorKind::TypeError);
        let kind = slot_kind(ctx, result)?;
        let value = self.load(ctx, kind, object, variant::VALUE);
        if perry_mir::absence(ctx, result).is_some_and(|a| a.admits_undefined()) {
            let undefined = self.undefined(ctx, result)?;
            Ok(self.select(ctx, present, value, undefined))
        } else {
            Ok(value)
        }
    }
}
