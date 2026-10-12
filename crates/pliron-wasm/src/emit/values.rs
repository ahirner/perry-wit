//! Instruction selection for statements and the value trees folded into them.
use super::{EmitResult, function::FunctionEmitter, schedule::movable};
use crate::{
    ops::{CallOp, LoadOp, NumOp, SelectOp, StoreOp},
    types::val_type,
};
use pliron::{
    builtin::{
        attributes::{FPDoubleAttr, FPSingleAttr, IntegerAttr},
        op_interfaces::OneResultInterface,
        ops::ConstantOp,
    },
    context::Ptr,
    operation::Operation,
    r#type::Typed,
    utils::apint::APInt,
    value::Value,
};
use wasm_encoder::{Instruction, MemArg, ValType};

impl FunctionEmitter<'_> {
    /// Emit `op` as a statement: evaluate its tree and store or drop its results. Ops
    /// evaluated in place at their user, and unused movable ones, emit nothing here.
    pub(super) fn statement(&mut self, op: Ptr<Operation>) -> EmitResult<()> {
        let ctx = self.ctx;
        if self.in_place(op) {
            return Ok(());
        }
        let results: Vec<_> = op.deref(ctx).results().collect();
        if movable(ctx, op) && results.iter().all(|v| !v.is_used(ctx)) {
            return Ok(());
        }
        self.tree(op)?;
        for result in results.iter().rev() {
            match self.locals.get(result) {
                Some(local) => self.code.push(Instruction::LocalSet(*local)),
                None => self.code.push(Instruction::Drop),
            }
        }
        Ok(())
    }

    /// Push `value` onto the operand stack.
    pub(super) fn value(&mut self, value: Value) -> EmitResult<()> {
        if let Some(def) = value.defining_op()
            && self.in_place(def)
        {
            return self.tree(def);
        }
        let local = *self
            .locals
            .get(&value)
            .ok_or_else(|| self.malformed("a used value has no local"))?;
        self.code.push(Instruction::LocalGet(local));
        Ok(())
    }

    /// Push `op`'s operands, then emit `op` itself.
    fn tree(&mut self, op: Ptr<Operation>) -> EmitResult<()> {
        let ctx = self.ctx;
        let operands: Vec<_> = op.deref(ctx).operands().collect();
        for operand in operands {
            self.value(operand)?;
        }
        let instruction = if let Some(constant) = Operation::get_op::<ConstantOp>(op, ctx) {
            let attr = constant
                .get_attr_builtin_constant_value(ctx)
                .ok_or_else(|| self.malformed("constant without value"))?;
            if let Some(int) = attr.downcast_ref::<IntegerAttr>() {
                let bits: APInt = int.clone().into();
                match val_type(ctx, constant.get_result(ctx).get_type(ctx)) {
                    Some(ValType::I32) => Instruction::I32Const(bits.to_u64() as u32 as i32),
                    Some(ValType::I64) => Instruction::I64Const(bits.to_u64() as i64),
                    _ => return Err(self.malformed("integer constant of a non-Wasm width")),
                }
            } else if let Some(double) = attr.downcast_ref::<FPDoubleAttr>() {
                Instruction::F64Const(f64::from(double.clone()).into())
            } else if let Some(single) = attr.downcast_ref::<FPSingleAttr>() {
                Instruction::F32Const(f32::from(single.clone()).into())
            } else {
                return Err(self.malformed("unsupported constant attribute"));
            }
        } else if let Some(num) = Operation::get_op::<NumOp>(op, ctx) {
            num.opcode(ctx).instruction()
        } else if Operation::get_op::<SelectOp>(op, ctx).is_some() {
            Instruction::Select
        } else if let Some(load) = Operation::get_op::<LoadOp>(op, ctx) {
            let kind = load.kind(ctx);
            kind.load(memarg(load.offset(ctx), kind.bytes()))
        } else if let Some(store) = Operation::get_op::<StoreOp>(op, ctx) {
            let kind = store.kind(ctx);
            kind.store(memarg(store.offset(ctx), kind.bytes()))
        } else if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
            let callee = call.callee(ctx);
            let index = *self
                .function_index
                .get(&callee)
                .ok_or_else(|| self.malformed(format!("unknown callee {callee}")))?;
            Instruction::Call(index)
        } else {
            return Err(self.unsupported(op));
        };
        self.code.push(instruction);
        Ok(())
    }
}

/// A naturally aligned access of `bytes` at `offset` in memory 0.
fn memarg(offset: u32, bytes: u32) -> MemArg {
    MemArg {
        offset: offset.into(),
        align: bytes.trailing_zeros(),
        memory_index: 0,
    }
}
