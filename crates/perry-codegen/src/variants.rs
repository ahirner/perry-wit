//! Compact finite variants: tag string, traced payload owner, and one eight-byte value slot.
use crate::{
    function::FunctionLowering,
    repr::{Repr, repr, slot_kind},
    runtime::Runtime,
};
use anyhow::Result;
use perry_abi::layout::variant;
use perry_mir::{types::VariantType, variants::admits_payload};
use pliron::{
    basic_block::BasicBlock,
    context::{Context, Ptr},
    r#type::{TypeHandle, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

/// The loop that writes a source-backed variant's members in their parsed order.
#[derive(Clone, Copy)]
struct MemberOrder {
    /// Loop header, taking the member `index`.
    header: Ptr<BasicBlock>,
    index: Value,
    /// The member the runtime writes next: 0 `"tag"`, 1 `"val"`, otherwise none.
    selected: Value,
    /// Continues the loop with the next member.
    advance: Ptr<BasicBlock>,
    finished: Ptr<BasicBlock>,
    /// Entered when `selected` is not the tag.
    rest: Ptr<BasicBlock>,
}

impl FunctionLowering<'_, '_> {
    pub(super) fn number_string_equal(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        a: Value,
        b: Value,
    ) -> Result<Value> {
        let result_type = crate::repr::i32_type(ctx);
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
        // Only programs that attach parse sources to typed values reserve the source slot.
        let size = self.i32_const(
            ctx,
            if self.state.json_provenance {
                variant::SIZE as i32
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
            self.store(ctx, MemKind::I32, object, variant::SOURCE, zero);
        }
        if let Some((ty, value)) = payload {
            self.store(ctx, slot_kind(ctx, ty)?, object, variant::VALUE, value);
        }
        Ok(object)
    }

    /// The members `"tag"` and, for a case with a payload, `"val"` of variant `value`. With
    /// JSON provenance, a loop writes them in the parsed member order of the value's source.
    pub(super) fn json_variant(
        &mut self,
        ctx: &mut Context,
        writer: Value,
        value: Value,
        ty: TypedHandle<VariantType>,
    ) -> Result<()> {
        let payload_cases: Vec<(String, TypeHandle)> = ty
            .deref(ctx)
            .cases()
            .iter()
            .filter_map(|(name, payload)| payload.map(|payload| (name.clone(), payload)))
            .collect();
        let tag = self.load(ctx, MemKind::I32, value, variant::TAG);
        let ordered = if self.state.json_provenance {
            Some(self.member_order(ctx, writer, value))
        } else {
            None
        };
        let key = self.state.data.string("\"tag\":");
        let key = self.i32_const(ctx, key as i32);
        self.runtime_void(ctx, Runtime::StdJsonKey, vec![writer, key]);
        self.runtime_void(ctx, Runtime::StdJsonQuote, vec![writer, tag]);
        if let Some(order) = ordered {
            self.br(ctx, order.advance, vec![]);
            self.block = order.rest;
            let one = self.i32_const(ctx, 1);
            let is_payload = self.num(ctx, NumOpcode::I32Eq, vec![order.selected, one]);
            let payload = self.new_block(ctx, vec![]);
            self.cond_br(ctx, is_payload, (payload, vec![]), (order.finished, vec![]));
            self.block = payload;
        }
        let done = self.new_block(ctx, vec![]);
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
        if let Some(order) = ordered {
            self.br(ctx, order.advance, vec![]);
            self.block = order.advance;
            let one = self.i32_const(ctx, 1);
            let index = self.num(ctx, NumOpcode::I32Add, vec![order.index, one]);
            self.br(ctx, order.header, vec![index]);
            self.block = order.finished;
        }
        Ok(())
    }

    /// Open the loop over the parsed members of variant `value` and continue in the block that
    /// writes `"tag"`, selected when the runtime reports member 0 next.
    fn member_order(&mut self, ctx: &mut Context, writer: Value, value: Value) -> MemberOrder {
        let source = self.load(ctx, MemKind::I32, value, variant::SOURCE);
        let members = self.runtime(ctx, Runtime::StdJsonSourceBegin, vec![source]);
        let i32 = crate::repr::i32_type(ctx);
        let header = self.new_block(ctx, vec![i32]);
        let advance = self.new_block(ctx, vec![]);
        let finished = self.new_block(ctx, vec![]);
        let zero = self.i32_const(ctx, 0);
        self.br(ctx, header, vec![zero]);
        self.block = header;
        let index = header.deref(ctx).get_argument(0);
        let count = self.i32_const(ctx, 2);
        let selected = self.runtime(
            ctx,
            Runtime::StdJsonSourceNext,
            vec![members, writer, index, count],
        );
        let tag_case = self.new_block(ctx, vec![]);
        let rest = self.new_block(ctx, vec![]);
        let is_tag = self.num(ctx, NumOpcode::I32Eqz, vec![selected]);
        self.cond_br(ctx, is_tag, (tag_case, vec![]), (rest, vec![]));
        self.block = tag_case;
        MemberOrder {
            header,
            index,
            selected,
            advance,
            finished,
            rest,
        }
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
