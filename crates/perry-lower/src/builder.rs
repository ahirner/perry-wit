//! The per-function MIR builder: its state and the IR plumbing every lowering uses.
use super::*;

pub(super) enum Control {
    Loop {
        continue_to: Ptr<BasicBlock>,
        break_to: Ptr<BasicBlock>,
    },
    Switch {
        break_to: Ptr<BasicBlock>,
    },
}

/// The innermost enclosing `catch` or exceptional `finally` path: throwing ops store their
/// status in `status` and branch to `block`.
#[derive(Clone, Copy)]
pub(super) struct Handler {
    pub(super) block: Ptr<BasicBlock>,
    pub(super) status: Value,
}

/// An enclosing `finally`, run inline before `return`, `break` and `continue` leave it.
#[derive(Clone)]
pub(super) struct Finally {
    pub(super) body: Vec<Stmt>,
    /// `handlers.len()` and `controls.len()` outside the `try`.
    pub(super) handlers: usize,
    pub(super) controls: usize,
}

/// Status of `process.exit` (`ErrorKind::Exit`): handlers pass it on.
pub(super) const EXIT_STATUS: u64 = 5;

pub(super) struct FunctionBuilder<'s> {
    pub(super) region: Ptr<pliron::region::Region>,
    pub(super) ins: IRInserter<DummyListener>,
    pub(super) block: Ptr<BasicBlock>,
    pub(super) locals: HashMap<LocalId, Value>,
    pub(super) global_slots: HashSet<Value>,
    /// Iterator-protocol guards of array destructuring that are statically false: the
    /// branches they select are not lowered.
    pub(super) false_guards: HashSet<LocalId>,
    /// Tag guards in effect, appended as conditions narrow; branches truncate back.
    pub(super) variant_guards: Vec<(variants::Path, String, bool)>,
    pub(super) controls: Vec<Control>,
    pub(super) callback_returns: Vec<array_methods::CallbackReturn>,
    pub(super) handlers: Vec<Handler>,
    pub(super) finallies: Vec<Finally>,
    pub(super) signatures: &'s HashMap<FuncId, Signature>,
    pub(super) shapes: &'s Shapes<'s>,
    /// Imported built-ins by local name.
    pub(super) builtins: &'s HashMap<String, Builtin>,
    pub(super) results: Vec<TypeHandle>,
    pub(super) is_async: bool,
    pub(super) labels: usize,
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn lower(
        ctx: &mut Context,
        function: &Function,
        signatures: &'s HashMap<FuncId, Signature>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<String, Builtin>,
    ) -> Result<FuncOp> {
        ensure!(
            function
                .captures
                .iter()
                .all(|id| shapes.globals.borrow().contains_key(id)),
            "closures capturing activation-local values are unsupported: {}",
            function.name
        );
        let signature = &signatures[&function.id];
        let params: Vec<LocalId> = function.params.iter().map(|p| p.id).collect();
        Self::lower_body(
            ctx,
            signature,
            &params,
            &function.body,
            signatures,
            shapes,
            builtins,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn lower_body(
        ctx: &mut Context,
        signature: &Signature,
        params: &[LocalId],
        body: &[Stmt],
        signatures: &'s HashMap<FuncId, Signature>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<String, Builtin>,
    ) -> Result<FuncOp> {
        let ty = FunctionType::get(ctx, signature.params.clone(), signature.results.clone());
        let func = FuncOp::new(ctx, signature.name.clone(), ty);
        let entry = func.get_entry_block(ctx);
        let region = func.get_operation().deref(ctx).get_region(0);
        let mut builder = Self::new(ctx, &func, signature, signatures, shapes, builtins);
        let mut globals: Vec<_> = shapes
            .globals
            .borrow()
            .iter()
            .map(|(&id, &ty)| (id, ty))
            .collect();
        globals.sort_by_key(|(id, _)| *id);
        for (id, ty) in globals {
            let slot = builder.emit_value(ctx, |ctx| {
                perry_mir::globals::GlobalOp::new(
                    ctx,
                    format!("global_{id}").try_into().expect("global name"),
                    ty,
                )
            });
            builder.locals.insert(id, slot);
            builder.global_slots.insert(slot);
        }
        for (index, param) in params.iter().enumerate() {
            let value = entry.deref(ctx).get_argument(index);
            let slot = builder.slot(ctx, value.get_type(ctx));
            builder.store(ctx, slot, value);
            builder.locals.insert(*param, slot);
        }
        builder.statements(ctx, body)?;
        if !builder.terminated(ctx) {
            let values = builder.implicit_results(ctx);
            builder.emit(ctx, |ctx| ReturnOp::new(ctx, values));
        }
        remove_unreachable(ctx, region);
        Ok(func)
    }

    pub(super) fn new(
        ctx: &Context,
        func: &FuncOp,
        signature: &Signature,
        signatures: &'s HashMap<FuncId, Signature>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<String, Builtin>,
    ) -> Self {
        let entry = func.get_entry_block(ctx);
        let region = func.get_operation().deref(ctx).get_region(0);
        Self {
            region,
            ins: IRInserter::new_at_block_end(entry),
            block: entry,
            locals: HashMap::new(),
            global_slots: HashSet::new(),
            false_guards: HashSet::new(),
            variant_guards: Vec::new(),
            controls: Vec::new(),
            callback_returns: Vec::new(),
            handlers: Vec::new(),
            finallies: Vec::new(),
            signatures,
            shapes,
            builtins,
            results: signature.results.clone(),
            is_async: signature.is_async,
            labels: 0,
        }
    }
}

impl FunctionBuilder<'_> {
    /// Results of falling off the end or `return;`: an async function settles `undefined`.
    pub(super) fn implicit_results(&mut self, ctx: &mut Context) -> Vec<Value> {
        self.results
            .clone()
            .into_iter()
            .map(|ty| match optional_value(ctx, ty) {
                Some(inner) if self.is_async => {
                    self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, inner))
                }
                _ => self.emit_value(ctx, |ctx| UndefOp::new(ctx, ty)),
            })
            .collect()
    }

    // ── IR plumbing ────────────────────────────────────────────────────────

    pub(super) fn append(&mut self, ctx: &mut Context, op: &dyn Op) {
        self.ins.append_op(ctx, op);
        self.guard(ctx, op.get_operation());
    }

    pub(super) fn append_value<T: Op + OneResultInterface>(
        &mut self,
        ctx: &mut Context,
        op: &T,
    ) -> Value {
        self.ins.append_op(ctx, op);
        self.guard(ctx, op.get_operation());
        op.get_result(ctx)
    }

    pub(super) fn emit<T: Op>(
        &mut self,
        ctx: &mut Context,
        make: impl FnOnce(&mut Context) -> T,
    ) -> T {
        let op = make(ctx);
        self.ins.append_op(ctx, &op);
        self.guard(ctx, op.get_operation());
        op
    }

    /// Inside a `try`, route the exception of an op that may throw to the handler.
    pub(super) fn guard(&mut self, ctx: &mut Context, op: Ptr<Operation>) {
        let Some(handler) = self.handlers.last().copied() else {
            return;
        };
        let throws = may_throw(ctx, op) && Operation::get_op::<RethrowOp>(op, ctx).is_none();
        if !throws {
            return;
        }
        let caught = CaughtOp::new(ctx);
        self.ins.append_op(ctx, &caught);
        let status = caught.get_result(ctx);
        let zero = self.i32(ctx, 0);
        let failed = MirCompare::new(ctx, CompareKind::Ne, status, zero);
        self.ins.append_op(ctx, &failed);
        let failed = failed.get_result(ctx);
        let land = self.new_block(ctx, "throw");
        let next = self.new_block(ctx, "caught");
        let branch = CondBrOp::new(ctx, failed, land, vec![], next, vec![]);
        self.ins.append_op(ctx, &branch);
        self.switch_to(land);
        let store = StoreOp::new(ctx, handler.status, status);
        self.ins.append_op(ctx, &store);
        let br = BrOp::new(ctx, handler.block, vec![]);
        self.ins.append_op(ctx, &br);
        self.switch_to(next);
    }

    pub(super) fn i32(&mut self, ctx: &mut Context, value: u64) -> Value {
        let ty = IntegerType::get(ctx, 32, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(value, bw(32)));
        let op = ConstantOp::new(ctx, Box::new(attr));
        self.ins.append_op(ctx, &op);
        op.get_result(ctx)
    }

    pub(super) fn emit_value<T: Op + OneResultInterface>(
        &mut self,
        ctx: &mut Context,
        make: impl FnOnce(&mut Context) -> T,
    ) -> Value {
        let op = self.emit(ctx, make);
        op.get_result(ctx)
    }

    pub(super) fn new_block(&mut self, ctx: &mut Context, label: &str) -> Ptr<BasicBlock> {
        self.labels += 1;
        let name = Identifier::try_from(format!("{label}{}", self.labels)).expect("label");
        let block = BasicBlock::new(ctx, Some(name), vec![]);
        block.insert_at_back(self.region, ctx);
        block
    }

    pub(super) fn switch_to(&mut self, block: Ptr<BasicBlock>) {
        self.block = block;
        self.ins.set_insertion_point_to_block_end(block);
    }

    pub(super) fn terminated(&self, ctx: &Context) -> bool {
        self.block.deref(ctx).get_terminator(ctx).is_some()
    }

    /// Branch to `target` unless the current block already ended (e.g. with `return`).
    pub(super) fn jump(&mut self, ctx: &mut Context, target: Ptr<BasicBlock>) {
        if !self.terminated(ctx) {
            self.emit(ctx, |ctx| BrOp::new(ctx, target, vec![]));
        }
    }

    /// After `return`/`break`/`continue`, keep lowering into an unreachable block.
    pub(super) fn after_terminator(&mut self, ctx: &mut Context) {
        let dead = self.new_block(ctx, "unreachable");
        self.switch_to(dead);
    }

    pub(super) fn slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        self.emit_value(ctx, |ctx| LocalOp::new(ctx, ty))
    }

    pub(super) fn store(&mut self, ctx: &mut Context, slot: Value, value: Value) {
        self.emit(ctx, |ctx| StoreOp::new(ctx, slot, value));
    }

    pub(super) fn load(&mut self, ctx: &mut Context, slot: Value) -> Result<Value> {
        if self.global_slots.contains(&slot) {
            let op = perry_mir::globals::GlobalLoadOp::new(ctx, slot)?;
            return Ok(self.append_value(ctx, &op));
        }
        let op = LoadOp::new(ctx, slot)?;
        Ok(self.append_value(ctx, &op))
    }

    pub(super) fn local(&self, id: LocalId) -> Result<Value> {
        self.locals.get(&id).copied().ok_or_else(|| {
            anyhow!(
                "unsupported reference to local {id} (globals and captures are not supported yet)"
            )
        })
    }

    pub(super) fn number(&mut self, ctx: &mut Context, value: f64) -> Value {
        let op = ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(value)));
        self.append_value(ctx, &op)
    }

    pub(super) fn boolean(&mut self, ctx: &mut Context, value: bool) -> Value {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless);
        let attr = IntegerAttr::new(ty, APInt::from_u64(u64::from(value), bw(1)));
        let op = ConstantOp::new(ctx, Box::new(attr));
        self.append_value(ctx, &op)
    }

    pub(super) fn binary(
        &mut self,
        ctx: &mut Context,
        kind: BinaryKind,
        lhs: Value,
        rhs: Value,
    ) -> Value {
        self.emit_value(ctx, |ctx| MirBinary::new(ctx, kind, lhs, rhs))
    }

    pub(super) fn compare(
        &mut self,
        ctx: &mut Context,
        kind: CompareKind,
        lhs: Value,
        rhs: Value,
    ) -> Value {
        self.emit_value(ctx, |ctx| MirCompare::new(ctx, kind, lhs, rhs))
    }

    pub(super) fn branch(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: Ptr<BasicBlock>,
        otherwise: Ptr<BasicBlock>,
    ) {
        self.emit(ctx, |ctx| {
            CondBrOp::new(ctx, condition, then, vec![], otherwise, vec![])
        });
    }

    pub(super) fn string(&mut self, ctx: &mut Context, text: &str) -> Value {
        let op = StringConstOp::new(ctx, text);
        self.append_value(ctx, &op)
    }

    /// A slot inserted at the start of the entry block, dominating every use.
    pub(super) fn entry_slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        let entry = self.region.deref(ctx).get_head().expect("entry block");
        let op = LocalOp::new(ctx, ty);
        let mut at_entry = IRInserter::<DummyListener>::new_at_block_start(entry);
        at_entry.append_op(ctx, &op);
        op.get_result(ctx)
    }

    pub(super) fn slot_type(&self, ctx: &Context, slot: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<perry_mir::types::SlotType>::from_handle(slot.get_type(ctx), ctx)
                .map_err(|_| anyhow!("not a slot"))?
                .deref(ctx)
                .value(),
        )
    }

    pub(super) fn array_element(&self, ctx: &Context, array: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
                .map_err(|_| anyhow!("expected an array"))?
                .deref(ctx)
                .element(),
        )
    }

    /// Emit `intrinsic` and return its result, if its signature has one: for dispatch
    /// over intrinsics and methods that may or may not produce a value. Otherwise use
    /// [Self::intrinsic] or [Self::intrinsic_void].
    pub(super) fn call_intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Option<Value> {
        let op = self.emit(ctx, |ctx| IntrinsicOp::new(ctx, intrinsic, args));
        let operation = op.get_operation().deref(ctx);
        (operation.get_num_results() == 1).then(|| operation.get_result(0))
    }

    /// Emit an intrinsic that returns a value.
    pub(super) fn intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Value {
        let Some(result) = self.call_intrinsic(ctx, intrinsic, args) else {
            panic!("{} returns no value", intrinsic.name());
        };
        result
    }

    /// Emit an intrinsic without a result.
    pub(super) fn intrinsic_void(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) {
        assert!(
            intrinsic.types().1.is_empty(),
            "the result of {} is discarded",
            intrinsic.name()
        );
        self.call_intrinsic(ctx, intrinsic, args);
    }

    /// Pick `then` or `otherwise` (strings) by `condition`, through a slot.
    pub(super) fn choose_string(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        otherwise: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
    ) -> Result<Value> {
        let ty = StringType::get(ctx).into();
        self.choose(ctx, ty, condition, then, otherwise)
    }

    /// A value of type `ty` from one of two branches.
    pub(super) fn choose(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        condition: Value,
        then: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        otherwise: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
    ) -> Result<Value> {
        let slot = self.entry_slot(ctx, ty);
        let then_block = self.new_block(ctx, "then");
        let else_block = self.new_block(ctx, "else");
        let join = self.new_block(ctx, "join");
        self.branch(ctx, condition, then_block, else_block);
        self.switch_to(then_block);
        let value = then(self, ctx)?;
        self.store(ctx, slot, value);
        self.jump(ctx, join);
        self.switch_to(else_block);
        let value = otherwise(self, ctx)?;
        self.store(ctx, slot, value);
        self.jump(ctx, join);
        self.switch_to(join);
        self.load(ctx, slot)
    }
}

/// Delete blocks no path from the entry reaches (code after `return`, handlers nothing throws
/// to); they would break dominance of the values they use.
pub(super) fn remove_unreachable(ctx: &mut Context, region: Ptr<pliron::region::Region>) {
    let blocks: Vec<_> = region.deref(ctx).iter(ctx).collect();
    let mut reached = HashSet::from([blocks[0]]);
    let mut stack = vec![blocks[0]];
    while let Some(block) = stack.pop() {
        for succ in block.deref(ctx).succs(ctx) {
            if reached.insert(succ) {
                stack.push(succ);
            }
        }
    }
    let dead: Vec<_> = blocks
        .into_iter()
        .filter(|b| !reached.contains(b))
        .collect();
    for &block in &dead {
        BasicBlock::drop_all_uses(block, ctx);
    }
    for block in dead {
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        for op in ops.into_iter().rev() {
            Operation::erase(op, ctx);
        }
    }
    let dead: Vec<_> = region
        .deref(ctx)
        .iter(ctx)
        .filter(|b| !reached.contains(b))
        .collect();
    for block in dead {
        BasicBlock::erase(block, ctx);
    }
}
