//! Wasm lowering of one MIR operation: dispatch to the domain lowerings, and directly the
//! scalar, constant, global, control-flow, call and promise operations.
use crate::{
    function::FunctionLowering,
    regex,
    repr::{Repr, i32_type, optional_inner, repr, slot_kind},
    runtime::Runtime,
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_mir::{
    ArrayType, AsyncCallOp, AwaitOp, BinaryOp as MirBinaryOp, BrOp as MirBrOp, CallOp as MirCallOp,
    Combinator, CompareKind, CompareOp as MirCompareOp, ConcatOp, CondBrOp as MirCondBrOp,
    ErrorKind, IntrinsicOp, JsonStringifyOp, NumericCastOp, PromiseCombinatorOp, PromiseRejectedOp,
    PromiseResolvedOp, PromiseType, ReturnOp as MirReturnOp, SettledField, SettledFieldOp,
    SettledType, StringConstOp, StringLenOp, StringType, TimerOp, UndefOp,
};
use pliron::{
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
        op_interfaces::BranchOpInterface,
        ops::ConstantOp,
    },
    context::{Context, Ptr},
    op::Op,
    operation::Operation,
    r#type::{Typed, TypedHandle},
    utils::apint::APInt,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    ops::CallOp,
};

impl FunctionLowering<'_, '_> {
    pub(super) fn op(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        if self.aggregate_op(ctx, op)?
            || self.optional_op(ctx, op)?
            || self.exception_op(ctx, op)?
        {
            return Ok(());
        }
        if let Some(global) = Operation::get_op::<perry_mir::GlobalOp>(op, ctx) {
            let ty = TypedHandle::<perry_mir::SlotType>::from_handle(
                op.deref(ctx).get_result(0).get_type(ctx),
                ctx,
            )?
            .deref(ctx)
            .value();
            let name = global.name(ctx);
            let address = if let Some(&(address, existing)) = self.state.globals.get(&name) {
                ensure!(
                    existing == ty,
                    "module binding {name} has conflicting types"
                );
                address
            } else {
                let address = self.state.data.append(8, &[0; 16]);
                self.state.globals.insert(name, (address, ty));
                address
            };
            let cell = self.i32_const(ctx, address as i32);
            if repr(ctx, ty)? == Repr::Ref {
                self.runtime_void(ctx, Runtime::GcGlobal, vec![cell]);
            }
            self.define(ctx, op, cell);
            return Ok(());
        }
        if Operation::get_op::<perry_mir::GlobalLoadOp>(op, ctx).is_some() {
            let slot = self.operand(ctx, op, 0)?;
            let initialized = self.load(ctx, MemKind::I32, slot, 8);
            let missing = self.num(ctx, NumOpcode::I32Eqz, vec![initialized]);
            self.throw_if(ctx, missing, ErrorKind::TypeError);
            let kind = slot_kind(ctx, op.deref(ctx).get_result(0).get_type(ctx))?;
            let value = self.load(ctx, kind, slot, 0);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<perry_mir::StoreOp>(op, ctx).is_some() {
            let slot = self.operand(ctx, op, 0)?;
            let value = self.operand(ctx, op, 1)?;
            let kind = slot_kind(ctx, op.deref(ctx).get_operand(1).get_type(ctx))?;
            self.store(ctx, kind, slot, 0, value);
            let one = self.i32_const(ctx, 1);
            self.store(ctx, MemKind::I32, slot, 8, one);
            return Ok(());
        }
        if let Some(constant) = Operation::get_op::<perry_mir::RegexConstOp>(op, ctx) {
            let key = (constant.pattern(ctx), constant.flags(ctx));
            let address = if let Some(&address) = self.state.data.regexes.get(&key) {
                address
            } else {
                let table = regex::compile(&key.0, &key.1)?;
                let address = self.state.data.append(4, &table);
                self.state.data.regexes.insert(key, address);
                address
            };
            let value = self.i32_const(ctx, address as i32);
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(constant) = Operation::get_op::<perry_mir::BigIntConstOp>(op, ctx) {
            let magnitude = constant
                .magnitude(ctx)
                .context("BigInt constant without a u64 magnitude")?;
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&magnitude.to_le_bytes());
            let address = self.state.data.append(8, &bytes);
            let value = self.i32_const(ctx, address as i32);
            self.define(ctx, op, value);
            return Ok(());
        }
        if let Some(constant) = Operation::get_op::<ConstantOp>(op, ctx) {
            let attr = constant
                .get_attr_builtin_constant_value(ctx)
                .ok_or_else(|| anyhow!("constant without value"))?;
            if let Some(&number) = attr.downcast_ref::<perry_mir::NumericConstant>() {
                drop(attr);
                let value = self.numeric_constant(ctx, number);
                self.define_value(ctx, op, value);
                return Ok(());
            }
            let value = if let Some(double) = attr.downcast_ref::<FPDoubleAttr>() {
                let value = f64::from(double.clone());
                drop(attr);
                self.f64_const(ctx, value)
            } else if let Some(int) = attr.downcast_ref::<IntegerAttr>() {
                let ty = int.get_type().into();
                let bits: APInt = int.clone().into();
                drop(attr);
                let (width, signed) =
                    crate::numeric::integer(ctx, ty).expect("integer attribute type");
                if width == 64 {
                    self.i64_const(ctx, bits.to_u64())
                } else {
                    let value = if signed && width > 1 {
                        bits.to_i64() as i32
                    } else {
                        bits.to_u64() as i32
                    };
                    self.i32_const(ctx, value)
                }
            } else {
                bail!("unsupported constant")
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<UndefOp>(op, ctx).is_some() {
            let ty = op.deref(ctx).get_result(0).get_type(ctx);
            let value = crate::values::carriers(ctx, ty)?.try_map(|ty| Ok(self.zero(ctx, ty)))?;
            self.define_value(ctx, op, value);
            return Ok(());
        }
        if let Some(binary) = Operation::get_op::<MirBinaryOp>(op, ctx) {
            let value = self.binary(ctx, op, binary.kind(ctx))?;
            self.define_value(ctx, op, value);
            return Ok(());
        }
        if let Some(compare) = Operation::get_op::<MirCompareOp>(op, ctx) {
            let mir_ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let kind = compare.kind(ctx);
            if crate::values::is_numeric_type(ctx, mir_ty) {
                let value = self.numeric_comparison(ctx, op, kind)?;
                self.define(ctx, op, value);
                return Ok(());
            }
            let operands = self.operands(ctx, op)?;
            if mir_ty.deref(ctx).is::<StringType>()
                && let Some(value) = self.string_literal_comparison(ctx, op, &operands, kind)
            {
                self.define(ctx, op, value);
                return Ok(());
            }
            let inner = optional_inner(ctx, mir_ty).unwrap_or(mir_ty);
            let value = if inner.deref(ctx).is::<perry_mir::NumberStringType>() {
                let equal = self.number_string_equal(ctx, mir_ty, operands[0], operands[1])?;
                match kind {
                    CompareKind::Eq => equal,
                    CompareKind::Ne => self.num(ctx, NumOpcode::I32Eqz, vec![equal]),
                    _ => bail!("narrow number | string before ordered comparisons"),
                }
            } else if let Some(inner) = optional_inner(ctx, mir_ty)
                && repr(ctx, inner)? == Repr::F64
            {
                // Strict equality on `number | undefined`: undefined equals only undefined.
                let (a, b) = (operands[0], operands[1]);
                let bits_a = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![a]);
                let bits_b = self.num(ctx, NumOpcode::I64ReinterpretF64, vec![b]);
                let same = self.num(ctx, NumOpcode::I64Eq, vec![bits_a, bits_b]);
                let undefined = self.is_absent(ctx, a, mir_ty)?;
                let both = self.num(ctx, NumOpcode::I32And, vec![same, undefined]);
                let equal = self.num(ctx, NumOpcode::F64Eq, vec![a, b]);
                let eq = self.num(ctx, NumOpcode::I32Or, vec![both, equal]);
                match kind {
                    CompareKind::Eq => eq,
                    CompareKind::Ne => self.num(ctx, NumOpcode::I32Eqz, vec![eq]),
                    other => bail!("ordered comparison {other:?} on number | undefined"),
                }
            } else if inner.deref(ctx).is::<StringType>()
                || inner.deref(ctx).is::<perry_mir::BigIntType>()
            {
                let join = if optional_inner(ctx, mir_ty).is_some() {
                    let a = self.is_absent(ctx, operands[0], mir_ty)?;
                    let b = self.is_absent(ctx, operands[1], mir_ty)?;
                    let either = self.num(ctx, NumOpcode::I32Or, vec![a, b]);
                    let same = self.num(ctx, NumOpcode::I32Eq, operands.clone());
                    let sentinel_order = self.num(ctx, NumOpcode::I32Eqz, vec![same]);
                    let i32 = i32_type(ctx);
                    let join = self.new_block(ctx, vec![i32]);
                    let defined = self.new_block(ctx, vec![]);
                    self.cond_br(ctx, either, (join, vec![sentinel_order]), (defined, vec![]));
                    self.block = defined;
                    Some(join)
                } else {
                    None
                };
                let order = if inner.deref(ctx).is::<perry_mir::BigIntType>() {
                    self.runtime(ctx, Runtime::StdBigintCompare, operands)
                } else {
                    self.runtime(ctx, Runtime::StringCompare, operands)
                };
                let order = if let Some(join) = join {
                    self.br(ctx, join, vec![order]);
                    self.block = join;
                    join.deref(ctx).get_argument(0)
                } else {
                    order
                };
                let zero = self.i32_const(ctx, 0);
                let opcode = match kind {
                    CompareKind::Lt => NumOpcode::I32LtS,
                    CompareKind::Le => NumOpcode::I32LeS,
                    CompareKind::Gt => NumOpcode::I32GtS,
                    CompareKind::Ge => NumOpcode::I32GeS,
                    CompareKind::Eq => NumOpcode::I32Eq,
                    CompareKind::Ne => NumOpcode::I32Ne,
                };
                self.num(ctx, opcode, vec![order, zero])
            } else {
                self.scalar_comparison(ctx, kind, mir_ty, operands[0], operands[1])?
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<NumericCastOp>(op, ctx).is_some() {
            let operand = op.deref(ctx).get_operand(0);
            let source = operand.get_type(ctx);
            let target = op.deref(ctx).get_result(0).get_type(ctx);
            let value = self.mapped_value(ctx, operand)?;
            let value = self.cast_value(ctx, value, source, target)?;
            self.define_value(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<MirBrOp>(op, ctx).is_some() {
            let dest = self.blocks[&op.deref(ctx).get_successor(0)];
            let args = self.flatten_operands(ctx, op, 0)?;
            self.br(ctx, dest, args);
            return Ok(());
        }
        if let Some(cond_br) = Operation::get_op::<MirCondBrOp>(op, ctx) {
            let condition = self.operand(ctx, op, 0)?;
            let mut arm = |succ: usize| -> Result<_> {
                let dest = self.blocks[&op.deref(ctx).get_successor(succ)];
                let mut args = Vec::new();
                for value in cond_br.successor_operands(ctx, succ) {
                    args.extend(self.mapped_value(ctx, value)?.iter());
                }
                Ok((dest, args))
            };
            let then = arm(0)?;
            let otherwise = arm(1)?;
            self.cond_br(ctx, condition, then, otherwise);
            return Ok(());
        }
        if Operation::get_op::<MirReturnOp>(op, ctx).is_some() {
            let mut values = self.flatten_operands(ctx, op, 0)?;
            if self.frame.is_some() {
                self.settle_return(ctx, values);
                return Ok(());
            }
            if self.throws {
                values.push(self.i32_const(ctx, 0));
            }
            self.leave_roots(ctx);
            self.ret(ctx, values);
            return Ok(());
        }
        if let Some(call) = Operation::get_op::<perry_mir::ForeignOp>(op, ctx) {
            match call.phase(ctx) {
                perry_mir::Phase::Call | perry_mir::Phase::Start => {
                    self.foreign_call(ctx, op, &call.binding(ctx))?
                }
                perry_mir::Phase::Finish => self.foreign_finish(ctx, op, &call.binding(ctx))?,
            }
            return Ok(());
        }
        if let Some(call) = Operation::get_op::<MirCallOp>(op, ctx) {
            let callee = call.callee(ctx);
            let args = self.flatten_operands(ctx, op, 0)?;
            let mir_results: Vec<_> = op.deref(ctx).results().collect();
            let mut results =
                crate::values::signature(ctx, mir_results.iter().map(|v| v.get_type(ctx)))?;
            let callee_throws = self.state.may_throw(&callee);
            if callee_throws {
                results.push(i32_type(ctx));
            }
            let wasm_call = CallOp::new(ctx, callee, args, results).get_operation();
            self.insert(ctx, wasm_call);
            let values: Vec<_> = wasm_call.deref(ctx).results().collect();
            let result_count = values.len() - usize::from(callee_throws);
            self.bind_values(ctx, mir_results, values[..result_count].iter().copied())?;
            if callee_throws {
                let status = *values
                    .last()
                    .ok_or_else(|| anyhow!("throwing call has no status result"))?;
                let throw = self.throw_target(ctx, status);
                let next = self.new_block(ctx, vec![]);
                self.cond_br(ctx, status, throw, (next, vec![]));
                self.block = next;
            }
            return Ok(());
        }
        if let Some(constant) = Operation::get_op::<StringConstOp>(op, ctx) {
            let address = {
                let text = constant
                    .get_attr_perry_string_value(ctx)
                    .ok_or_else(|| anyhow!("string constant without value"))?;
                self.state.data.string(text.as_ref())
            };
            let value = self.i32_const(ctx, address as i32);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<perry_mir::ConcatReuseOp>(op, ctx).is_some() {
            if op
                .deref(ctx)
                .get_result(0)
                .get_type(ctx)
                .deref(ctx)
                .is::<perry_mir::ArrayType>()
            {
                return self.array_concat(ctx, op);
            }
            let args = self.operands(ctx, op)?;
            let value = self.runtime(ctx, Runtime::StringAppend, args);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<ConcatOp>(op, ctx).is_some() {
            if op
                .deref(ctx)
                .get_result(0)
                .get_type(ctx)
                .deref(ctx)
                .is::<perry_mir::ArrayType>()
            {
                return self.array_concat(ctx, op);
            }
            let operands = self.operands(ctx, op)?;
            let value = self.runtime(ctx, Runtime::StringConcat, operands);
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<StringLenOp>(op, ctx).is_some() {
            let string = self.operands(ctx, op)?[0];
            let value = self.load(
                ctx,
                MemKind::I32,
                string,
                perry_abi::layout::string::SCALARS,
            );
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<AwaitOp>(op, ctx).is_some() {
            return self.lower_await(ctx, op);
        }
        if let Some(combinator) = Operation::get_op::<PromiseCombinatorOp>(op, ctx) {
            self.state.runtime_frames.insert(
                perry_abi::layout::promise::COMBINATOR_KIND,
                Runtime::CombinatorStep,
            );
            let function = match combinator.combinator(ctx) {
                Combinator::All => Runtime::PromiseAll,
                Combinator::Race => Runtime::PromiseRace,
                Combinator::AllSettled => Runtime::PromiseAllSettled,
            };
            let inputs = self.operand(ctx, op, 0)?;
            let promise = if self.state.collectible {
                let array = TypedHandle::<ArrayType>::from_handle(
                    op.deref(ctx).get_operand(0).get_type(ctx),
                    ctx,
                )?;
                let element = array.deref(ctx).element();
                let payload = TypedHandle::<PromiseType>::from_handle(element, ctx)?
                    .deref(ctx)
                    .value();
                let references = self.i32_const(ctx, i32::from(repr(ctx, payload)? == Repr::Ref));
                let mode = self.i32_const(
                    ctx,
                    match combinator.combinator(ctx) {
                        Combinator::All => 0,
                        Combinator::Race => 1,
                        Combinator::AllSettled => 2,
                    },
                );
                self.runtime(
                    ctx,
                    Runtime::PromiseCombineCollectible,
                    vec![mode, inputs, references],
                )
            } else {
                self.runtime(ctx, function, vec![inputs])
            };
            self.define(ctx, op, promise);
            return Ok(());
        }
        if Operation::get_op::<PromiseResolvedOp>(op, ctx).is_some() {
            let value = self.operand(ctx, op, 0)?;
            let bits = self.slot_bits(ctx, value);
            let payload = TypedHandle::<PromiseType>::from_handle(
                op.deref(ctx).get_result(0).get_type(ctx),
                ctx,
            )?
            .deref(ctx)
            .value();
            let promise = self.new_promise(ctx, payload)?;
            self.runtime_void(ctx, Runtime::PromiseResolve, vec![promise, bits]);
            self.define(ctx, op, promise);
            return Ok(());
        }
        if let Some(read) = Operation::get_op::<SettledFieldOp>(op, ctx) {
            // A settled view is the input promise itself (`perry_promise_all_settled`).
            use perry_abi::layout::promise::{FULFILLED, STATE, VALUE};
            let promise = self.operand(ctx, op, 0)?;
            let state = self.load(ctx, MemKind::I32, promise, STATE);
            let fulfilled_state = self.i32_const(ctx, FULFILLED as i32);
            let fulfilled = self.num(ctx, NumOpcode::I32Eq, vec![state, fulfilled_state]);
            let value = match read.field(ctx) {
                SettledField::Status => {
                    let yes = self.state.data.string("fulfilled");
                    let no = self.state.data.string("rejected");
                    let yes = self.i32_const(ctx, yes as i32);
                    let no = self.i32_const(ctx, no as i32);
                    self.select(ctx, fulfilled, yes, no)
                }
                field => {
                    let ty = op.deref(ctx).get_result(0).get_type(ctx);
                    let inner = optional_inner(ctx, ty)
                        .ok_or_else(|| anyhow!("settled field needs an optional result"))?;
                    let loaded = self.load(ctx, slot_kind(ctx, inner)?, promise, VALUE);
                    let undefined = self.undefined(ctx, ty)?;
                    if field == SettledField::Value {
                        let source = TypedHandle::<SettledType>::from_handle(
                            op.deref(ctx).get_operand(0).get_type(ctx),
                            ctx,
                        )?
                        .deref(ctx)
                        .value();
                        if optional_inner(ctx, source).is_some() {
                            self.check_optional_cast(ctx, loaded.into(), source, ty)?;
                        }
                        self.select(ctx, fulfilled, loaded, undefined)
                    } else {
                        self.select(ctx, fulfilled, undefined, loaded)
                    }
                }
            };
            self.define(ctx, op, value);
            return Ok(());
        }
        if Operation::get_op::<perry_mir::JsonDecodeOp>(op, ctx).is_some()
            || Operation::get_op::<perry_mir::JsonCopyOp>(op, ctx).is_some()
        {
            return self.json_decode(ctx, op);
        }
        if Operation::get_op::<JsonStringifyOp>(op, ctx).is_some()
            || Operation::get_op::<perry_mir::JsonBoxOp>(op, ctx).is_some()
        {
            return self.json_stringify(ctx, op);
        }
        if let Some(intrinsic) = Operation::get_op::<IntrinsicOp>(op, ctx) {
            return self.intrinsic(ctx, op, intrinsic.intrinsic(ctx));
        }
        if Operation::get_op::<PromiseRejectedOp>(op, ctx).is_some() {
            let exception = self.thrown(ctx, op)?;
            let status = self.runtime(ctx, Runtime::ExceptionRaise, vec![exception]);
            let payload = TypedHandle::<PromiseType>::from_handle(
                op.deref(ctx).get_result(0).get_type(ctx),
                ctx,
            )?
            .deref(ctx)
            .value();
            let promise = self.new_promise(ctx, payload)?;
            self.runtime_void(ctx, Runtime::PromiseReject, vec![promise, status]);
            self.define(ctx, op, promise);
            return Ok(());
        }
        if Operation::get_op::<TimerOp>(op, ctx).is_some() {
            return self.lower_timer(ctx, op);
        }
        if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
            // The callee's ramp runs it to its first suspension and returns its promise.
            let callee = call.callee(ctx);
            let args = self.flatten_operands(ctx, op, 0)?;
            let i32 = i32_type(ctx);
            let ramp = CallOp::new(ctx, callee, args, vec![i32]).get_operation();
            let promise = self.result(ctx, ramp);
            self.define(ctx, op, promise);
            return Ok(());
        }
        let name = Operation::get_op_dyn(op, ctx).get_opid().to_string();
        bail!("{name} has no lowering to wasm (did canonicalize run?)")
    }
}
