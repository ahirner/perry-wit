//! Construction helpers of [FunctionLowering]: appending ops, constants, memory accesses,
//! calls and branches to the current block.
use crate::{function::FunctionLowering, host::Host, repr::i32_type, runtime::Runtime};
use anyhow::{Result, anyhow};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
        ops::ConstantOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    op::Op,
    operation::Operation,
    r#type::TypeHandle,
    utils::apint::APInt,
    value::Value,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::{BrOp, BrTableOp, CondBrOp},
    ops::{CallOp, LoadOp, NumOp, SelectOp, StoreOp},
};
use std::num::NonZeroUsize;

impl FunctionLowering<'_, '_> {
    pub(crate) fn new_block(
        &mut self,
        ctx: &mut Context,
        args: Vec<TypeHandle>,
    ) -> Ptr<BasicBlock> {
        let block = BasicBlock::new(ctx, None, args);
        block.insert_at_back(self.region, ctx);
        block
    }

    pub(crate) fn insert(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Ptr<Operation> {
        op.insert_at_back(self.block, ctx);
        op
    }

    pub(crate) fn result(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Value {
        self.insert(ctx, op).deref(ctx).get_result(0)
    }

    pub(crate) fn i32_const(&mut self, ctx: &mut Context, value: i32) -> Value {
        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
        let attr = IntegerAttr::new(
            ty,
            APInt::from_u64(
                value as u32 as u64,
                const { NonZeroUsize::new(32).unwrap() },
            ),
        );
        let op = ConstantOp::new(ctx, Box::new(attr)).get_operation();
        self.result(ctx, op)
    }

    pub(crate) fn i64_const(&mut self, ctx: &mut Context, value: u64) -> Value {
        let ty = IntegerType::get(ctx, 64, Signedness::Signless);
        let attr = IntegerAttr::new(
            ty,
            APInt::from_u64(value, const { NonZeroUsize::new(64).unwrap() }),
        );
        let op = ConstantOp::new(ctx, Box::new(attr)).get_operation();
        self.result(ctx, op)
    }

    pub(crate) fn f64_const(&mut self, ctx: &mut Context, value: f64) -> Value {
        let op = ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(value))).get_operation();
        self.result(ctx, op)
    }

    pub(crate) fn num(
        &mut self,
        ctx: &mut Context,
        opcode: NumOpcode,
        operands: Vec<Value>,
    ) -> Value {
        let op = NumOp::new(ctx, opcode, operands).get_operation();
        self.result(ctx, op)
    }

    pub(crate) fn select(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: Value,
        otherwise: Value,
    ) -> Value {
        let op = SelectOp::new(ctx, condition, then, otherwise).get_operation();
        self.result(ctx, op)
    }

    pub(crate) fn load(
        &mut self,
        ctx: &mut Context,
        kind: MemKind,
        address: Value,
        offset: u32,
    ) -> Value {
        let op = LoadOp::new(ctx, kind, address, offset).get_operation();
        self.result(ctx, op)
    }
    pub(crate) fn store(
        &mut self,
        ctx: &mut Context,
        kind: MemKind,
        address: Value,
        offset: u32,
        value: Value,
    ) {
        let op = StoreOp::new(ctx, kind, address, offset, value).get_operation();
        self.insert(ctx, op);
    }

    /// Call `function` and return its result, if its signature has one: for dispatch tables
    /// that mix functions with and without a result. Otherwise use [Self::runtime] or
    /// [Self::runtime_void].
    pub(crate) fn call_runtime(
        &mut self,
        ctx: &mut Context,
        function: Runtime,
        args: Vec<Value>,
    ) -> Option<Value> {
        self.state.runtime.insert(function);
        let (_, results) = function.signature(ctx);
        let op = CallOp::new(ctx, function.symbol(), args, results).get_operation();
        self.insert(ctx, op);
        op.deref(ctx).results().next()
    }

    /// Call a runtime function that returns a value. Results are declared statically in
    /// [Runtime], so a function without one is a bug at the call site.
    pub(crate) fn runtime(
        &mut self,
        ctx: &mut Context,
        function: Runtime,
        args: Vec<Value>,
    ) -> Value {
        self.call_runtime(ctx, function, args).unwrap_or_else(|| {
            panic!(
                "{} is declared without a result; call it with runtime_void",
                function.name()
            )
        })
    }

    /// Call a runtime function without a result.
    pub(crate) fn runtime_void(&mut self, ctx: &mut Context, function: Runtime, args: Vec<Value>) {
        assert!(
            function.result().is_none(),
            "the result of {} is discarded",
            function.name()
        );
        self.call_runtime(ctx, function, args);
    }

    pub(crate) fn br(&mut self, ctx: &mut Context, dest: Ptr<BasicBlock>, args: Vec<Value>) {
        let op = BrOp::new(ctx, dest, args).get_operation();
        self.insert(ctx, op);
    }

    /// `br_table`: `targets[index]`, else `default` (blocks without arguments).
    pub(crate) fn br_table(
        &mut self,
        ctx: &mut Context,
        index: Value,
        targets: Vec<Ptr<BasicBlock>>,
        default: Ptr<BasicBlock>,
    ) {
        let op = BrTableOp::new(ctx, index, targets, default).get_operation();
        self.insert(ctx, op);
    }

    pub(crate) fn cond_br(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: (Ptr<BasicBlock>, Vec<Value>),
        otherwise: (Ptr<BasicBlock>, Vec<Value>),
    ) {
        let op =
            CondBrOp::new(ctx, condition, then.0, then.1, otherwise.0, otherwise.1).get_operation();
        self.insert(ctx, op);
    }

    /// The Wasm value of MIR `value`; frame-resident values are loaded from the frame.
    pub(crate) fn mapped(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if let Some(frame) = &self.frame
            && let Some(&(offset, kind)) = frame.slots.get(&value)
        {
            let frame = frame.frame;
            return Ok(self.load(ctx, kind, frame, offset));
        }
        self.values
            .get(&value)
            .copied()
            .ok_or_else(|| anyhow!("operand defined after its use"))
    }

    pub(crate) fn operand(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        index: usize,
    ) -> Result<Value> {
        let value = op.deref(ctx).get_operand(index);
        self.mapped(ctx, value)
    }

    pub(crate) fn operands(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<Vec<Value>> {
        let operands: Vec<_> = op.deref(ctx).operands().collect();
        operands.into_iter().map(|v| self.mapped(ctx, v)).collect()
    }

    /// Call a canonical built-in or host import.
    pub(crate) fn host(
        &mut self,
        ctx: &mut Context,
        host: Host,
        args: Vec<Value>,
    ) -> Option<Value> {
        let (_, results) = host.signature(ctx);
        let symbol = host.symbol();
        self.state.host.insert(host);
        let op = CallOp::new(ctx, symbol, args, results).get_operation();
        self.insert(ctx, op);
        op.deref(ctx).results().next()
    }

    pub(crate) fn define(&mut self, ctx: &Context, op: Ptr<Operation>, value: Value) {
        self.values.insert(op.deref(ctx).get_result(0), value);
    }

    /// Call `function` (no parameters, an `i32` result) at the start of the entry block, so
    /// that its result dominates every block, including ones created before this call.
    pub(crate) fn entry_runtime(&mut self, ctx: &mut Context, function: Runtime) -> Value {
        self.state.runtime.insert(function);
        let ty = i32_type(ctx);
        let call = CallOp::new(ctx, function.symbol(), vec![], vec![ty]).get_operation();
        call.insert_at_front(self.entry, ctx);
        call.deref(ctx).get_result(0)
    }

    /// `for (i = 0; i < count; i++) body(i)` as a CFG; code after it continues in the exit.
    pub(crate) fn counted_loop(
        &mut self,
        ctx: &mut Context,
        count: Value,
        body: impl FnOnce(&mut Self, &mut Context, Value) -> Result<()>,
    ) -> Result<()> {
        let i32 = i32_type(ctx);
        let header = self.new_block(ctx, vec![i32]);
        let inside = self.new_block(ctx, vec![]);
        let exit = self.new_block(ctx, vec![]);
        let zero = self.i32_const(ctx, 0);
        self.br(ctx, header, vec![zero]);
        self.block = header;
        let i = header.deref(ctx).get_argument(0);
        let more = self.num(ctx, NumOpcode::I32LtU, vec![i, count]);
        self.cond_br(ctx, more, (inside, vec![]), (exit, vec![]));
        self.block = inside;
        body(self, ctx, i)?;
        let one = self.i32_const(ctx, 1);
        let next = self.num(ctx, NumOpcode::I32Add, vec![i, one]);
        self.br(ctx, header, vec![next]);
        self.block = exit;
        Ok(())
    }
}
