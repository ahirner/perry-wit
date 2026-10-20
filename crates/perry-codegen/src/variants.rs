//! Compact finite variants: tag string, traced payload owner, and one eight-byte value slot.
use crate::{
    function::FunctionLowering,
    repr::{Repr, i32_type, optional_inner, repr, slot_kind},
    runtime::Runtime,
};
use anyhow::Result;
use perry_abi::layout::variant;
use perry_mir::admits_payload;
use pliron::{context::Context, r#type::TypeHandle, value::Value};
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
