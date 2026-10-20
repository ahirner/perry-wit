use crate::{
    function::FunctionLowering,
    repr::{optional_inner, slot_size},
    runtime::Runtime,
};
use anyhow::Result;
use perry_mir::{ArrayType, JsonSchema, RecordType};
use pliron::{
    context::Context,
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::{MemKind, NumOpcode};

impl FunctionLowering<'_, '_> {
    pub(crate) fn json_decode(
        &mut self,
        ctx: &mut Context,
        op: pliron::context::Ptr<Operation>,
    ) -> Result<()> {
        let ty = op.deref(ctx).get_result(0).get_type(ctx);
        let copy = Operation::is_op::<perry_mir::JsonCopyOp>(op, ctx);
        let operand = op.deref(ctx).get_operand(0);
        let text = !copy
            && operand
                .get_type(ctx)
                .deref(ctx)
                .is::<perry_mir::StringType>();
        let source = self.mapped_value(ctx, operand)?;
        let input = if text {
            self.runtime(ctx, Runtime::JsonCursorBegin, vec![source.scalar()?])
        } else if copy {
            self.i32_const(ctx, 0)
        } else {
            source.scalar()?
        };
        let schema = self.layout_descriptor(ctx, ty)?;
        let (bits, failed, tag) = if copy {
            let plan = self.json_copy_plan(ctx, operand.get_type(ctx), ty)?;
            let plan = self.i32_const(ctx, plan as i32);
            let (bits, tag) = self.json_carrier_bits(ctx, operand.get_type(ctx), source);
            let position = self.i32_const(ctx, 0);
            let schema = self.i32_const(ctx, schema as i32);
            self.call_json_engine(
                ctx,
                Runtime::JsonDescriptorCopy,
                [schema, bits, plan, tag, position],
            )
        } else {
            let schema = self.i32_const(ctx, schema as i32);
            let mode = self.i32_const(ctx, i32::from(text));
            let bits = self.slot_bits(ctx, input);
            let zero = self.i32_const(ctx, 0);
            let decoder = if text {
                Runtime::JsonDescriptorCursor
            } else {
                Runtime::JsonDescriptorGraph
            };
            self.call_json_engine(ctx, decoder, [schema, bits, mode, zero, zero])
        };
        let status = if text {
            self.runtime_void(ctx, Runtime::JsonCursorFinish, vec![input, failed]);
            self.runtime(ctx, Runtime::Status, vec![])
        } else {
            failed
        };
        self.throw_status_if(ctx, status, status);
        let value = match crate::repr::repr(ctx, ty)? {
            crate::repr::Repr::F64 => self.num(ctx, NumOpcode::F64ReinterpretI64, vec![bits]),
            crate::repr::Repr::I64 => bits,
            _ => self.num(ctx, NumOpcode::I32WrapI64, vec![bits]),
        };
        if crate::values::is_numeric_type(ctx, ty) {
            self.define_value(
                ctx,
                op,
                crate::repr::Carriers::Numeric { tag, payload: bits },
            );
        } else {
            self.define(ctx, op, value);
        }
        Ok(())
    }

    fn call_json_engine(
        &mut self,
        ctx: &mut Context,
        decoder: Runtime,
        input: [Value; 5],
    ) -> (Value, Value, Value) {
        let address = if let Some(address) = self.state.json_result {
            address
        } else {
            let mut bytes = vec![0; 16];
            for message in [
                "undefined value is not valid in JSON at this position",
                "unexpected absent value in JSON copy",
                "non-finite number is not valid for non-nullable target schema",
                "JSON tag is not a case of the declared variant",
            ] {
                bytes.extend_from_slice(&self.state.data.string(message).to_le_bytes());
            }
            let address = self.state.data.append(8, &bytes);
            self.state.json_result = Some(address);
            address
        };
        let out = self.i32_const(ctx, address as i32);
        let failed = self.runtime(ctx, decoder, input.into_iter().chain([out]).collect());
        (
            self.load(ctx, MemKind::I64, out, 0),
            failed,
            self.load(ctx, MemKind::I32, out, 8),
        )
    }

    pub(super) fn json_copy_plan(
        &mut self,
        ctx: &Context,
        source: TypeHandle,
        target: TypeHandle,
    ) -> Result<u32> {
        if let Some(&plan) = self.state.json_copy_plans.get(&(source, target)) {
            return Ok(plan);
        }
        let target_desc = self.layout_descriptor(ctx, target)?;
        let source_desc = self.layout_descriptor(ctx, source)?;
        let source_inner = optional_inner(ctx, source).unwrap_or(source);
        let target_inner = optional_inner(ctx, target).unwrap_or(target);
        let count = match JsonSchema::of(ctx, target_inner) {
            Some(JsonSchema::Record(record)) => record.deref(ctx).fields()?.len(),
            Some(JsonSchema::Variant(variant)) => variant.deref(ctx).cases().len(),
            _ => 1,
        };
        let plan = self.state.data.append(4, &vec![0; 8 + 8 * count]);
        self.state.json_copy_plans.insert((source, target), plan);
        let mut words = Vec::with_capacity(2 + 2 * count);
        words.extend([target_desc, source_desc]);
        if let Ok(target) = TypedHandle::<RecordType>::from_handle(target_inner, ctx) {
            let source = TypedHandle::<RecordType>::from_handle(source_inner, ctx)?;
            let source = source.deref(ctx);
            for (name, ty) in target.deref(ctx).fields()? {
                let mut offset = perry_abi::layout::record::HEADER;
                let field = source.fields()?.iter().find_map(|(key, ty)| {
                    let slot = offset;
                    offset += slot_size(ctx, *ty);
                    (key == name).then_some((*ty, slot))
                });
                if let Some((from, offset)) = field {
                    words.extend([self.json_copy_plan(ctx, from, *ty)?, offset]);
                } else {
                    words.extend([0, 0]);
                }
            }
        } else if let Ok(target) = TypedHandle::<ArrayType>::from_handle(target_inner, ctx) {
            let source = TypedHandle::<ArrayType>::from_handle(source_inner, ctx)?;
            words.push(self.json_copy_plan(
                ctx,
                source.deref(ctx).element(),
                target.deref(ctx).element(),
            )?);
        } else if let Some(JsonSchema::Variant(variant)) = JsonSchema::of(ctx, target_inner) {
            for (_, ty) in variant.deref(ctx).cases() {
                words.extend([
                    ty.map(|ty| self.json_copy_plan(ctx, ty, ty))
                        .transpose()?
                        .unwrap_or(0),
                    0,
                ]);
            }
        }
        self.write_data_words(plan, words);
        Ok(plan)
    }
}
