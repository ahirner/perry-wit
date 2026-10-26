//! Compact finite variants: tag string, traced payload owner, and one eight-byte value slot.
use crate::{
    function::FunctionLowering,
    repr::{Repr, i32_type, optional_inner, repr, slot_kind},
    runtime::Runtime,
};
use anyhow::{Context as _, Result};
use perry_abi::layout::variant;
use perry_mir::admits_payload;
use pliron::{context::Context, r#type::TypeHandle, value::Value};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_, '_> {
    pub(super) fn value_union_equal(
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
        let inner = optional_inner(ctx, ty).unwrap_or(ty);
        let cases = perry_mir::variant_cases(ctx, inner)?;
        for (index, (tag, payload)) in cases.iter().enumerate() {
            let payload = payload.context("value union without a payload")?;
            let next = if index + 1 < cases.len() {
                let tag = self.state.data.string(tag);
                let tag = self.i32_const(ctx, tag as i32);
                let matches = self.num(ctx, NumOpcode::I32Eq, vec![tag_a, tag]);
                let active = self.new_block(ctx, vec![]);
                let next = self.new_block(ctx, vec![]);
                self.cond_br(ctx, matches, (active, vec![]), (next, vec![]));
                self.block = active;
                Some(next)
            } else {
                None
            };
            let kind = slot_kind(ctx, payload)?;
            let a = self.load(ctx, kind, a, variant::VALUE);
            let b = self.load(ctx, kind, b, variant::VALUE);
            let equal = if payload.deref(ctx).is::<perry_mir::StringType>() {
                let order = self.runtime(ctx, Runtime::StringCompare, vec![a, b]);
                self.num(ctx, NumOpcode::I32Eqz, vec![order])
            } else {
                let opcode = if repr(ctx, payload)? == Repr::F64 {
                    NumOpcode::F64Eq
                } else {
                    NumOpcode::I32Eq
                };
                self.num(ctx, opcode, vec![a, b])
            };
            self.br(ctx, done, vec![equal]);
            if let Some(next) = next {
                self.block = next;
            }
        }
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
