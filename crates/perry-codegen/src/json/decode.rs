use crate::{function::FunctionLowering, runtime::Runtime};
use perry_mir::{CopySlot, JsonCopyPlan, PlironResultExt};
use perry_wit_diagnostics::{Result, internal};
use pliron::{
    context::Context,
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use pliron_wasm::attrs::MemKind;

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
            let pair = (operand.get_type(ctx), ty);
            let plan = JsonCopyPlan::with(ctx, pair, |ctx, layout| {
                self.json_copy_plan(ctx, layout, pair)
            })
            .to_diag(ctx)?
            .ok_or_else(|| internal!("incompatible JSON copy types"))??;
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
        if crate::values::is_numeric_type(ctx, ty) {
            self.define_value(
                ctx,
                op,
                crate::repr::Carriers::Numeric { tag, payload: bits },
            );
        } else {
            let value = self.repack_bits(ctx, bits, crate::repr::wasm_type(ctx, ty)?);
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

    fn json_copy_plan(
        &mut self,
        ctx: &Context,
        layout: &JsonCopyPlan,
        pair: (TypeHandle, TypeHandle),
    ) -> Result<u32> {
        if let Some(&plan) = self.state.json_copy_plans.get(&pair) {
            return Ok(plan);
        }
        let slots = layout
            .slots(pair)
            .ok_or_else(|| internal!("missing JSON copy layout"))?;
        let target = self.layout_descriptor(ctx, pair.1)?;
        let source = self.layout_descriptor(ctx, pair.0)?;
        let plan = self.state.data.append(4, &vec![0; 8 + 8 * slots.len()]);
        self.state.json_copy_plans.insert(pair, plan);
        let mut words = vec![target, source];
        for slot in slots {
            let entry = if let Some(slot) = slot {
                let offset = match *slot {
                    CopySlot::Field(_, record, index) => {
                        self.state
                            .record_layouts
                            .get(ctx, record)?
                            .fields
                            .get_index(index)
                            .ok_or_else(|| internal!("missing JSON copy source field"))?
                            .1
                            .offset
                    }
                    CopySlot::Payload(_) => 0,
                };
                [self.json_copy_plan(ctx, layout, slot.types())?, offset]
            } else {
                [0, 0]
            };
            words.extend(entry);
        }
        self.write_data_words(plan, words);
        Ok(plan)
    }
}
