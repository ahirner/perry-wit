//! The per-function MIR builder: its state and the IR plumbing every lowering uses.
use super::*;
use perry_mir::{IntrinsicOp, LocalOp, StringConstOp};
use pliron::builtin::attributes::FPDoubleAttr;

pub(super) enum Control<'s> {
    Loop {
        label: Option<&'s str>,
        continue_to: Ptr<BasicBlock>,
        break_to: Ptr<BasicBlock>,
    },
    Switch {
        label: Option<&'s str>,
        break_to: Ptr<BasicBlock>,
    },
    Labeled {
        label: &'s str,
        break_to: Ptr<BasicBlock>,
    },
}

#[derive(Clone, Copy)]
pub(super) enum Jump {
    Break,
    Continue,
}

impl Control<'_> {
    pub(super) fn target(&self, jump: Jump, label: Option<&str>) -> Option<Ptr<BasicBlock>> {
        let (own_label, break_to, continue_to) = match self {
            Self::Loop {
                label,
                break_to,
                continue_to,
            } => (*label, *break_to, Some(*continue_to)),
            Self::Switch { label, break_to } => (*label, *break_to, None),
            Self::Labeled { label, break_to } => (Some(*label), *break_to, None),
        };
        if let Some(label) = label {
            if own_label != Some(label) {
                return None;
            }
        } else if matches!(self, Self::Labeled { .. }) {
            return None;
        }
        match jump {
            Jump::Break => Some(break_to),
            Jump::Continue => continue_to,
        }
    }
}

/// The innermost enclosing `catch` or exceptional `finally` path: throwing ops store their
/// status in `status` and branch to `block`.
#[derive(Clone, Copy)]
pub(super) struct Handler {
    pub(super) block: Ptr<BasicBlock>,
    pub(super) status: Value,
}

/// An enclosing `finally`, run inline before `return`, `break` and `continue` leave it.
#[derive(Clone, Copy)]
pub(super) struct Finally<'s> {
    pub(super) body: &'s [Stmt],
    /// `handlers.len()` and `controls.len()` outside the `try`.
    pub(super) handlers: usize,
    pub(super) controls: usize,
}

/// Status of `process.exit` (`ErrorKind::Exit`): handlers pass it on.
pub(super) const EXIT_STATUS: u64 = 5;

/// A local or global slot and its optional `perry:types` integer wrapping constraint.
#[derive(Clone, Copy)]
pub(super) struct LocalBinding {
    pub(super) slot: Value,
    pub(super) integer: Option<TypedHandle<IntegerType>>,
    pub(super) is_never: bool,
}

impl LocalBinding {
    pub(super) const fn plain(slot: Value) -> Self {
        Self {
            slot,
            integer: None,
            is_never: false,
        }
    }
}

pub(super) struct FunctionBuilder<'s> {
    pub(super) region: Ptr<pliron::region::Region>,
    /// The region's entry block, where slots are allocated so they dominate every use.
    entry: Ptr<BasicBlock>,
    pub(super) ins: IRInserter<DummyListener>,
    pub(super) block: Ptr<BasicBlock>,
    pub(super) locals: bindings::Bindings<LocalBinding>,
    pub(super) closures: bindings::Bindings<&'s Expr>,
    pub(super) never_values: HashSet<Value>,
    pub(super) global_slots: HashSet<Value>,
    /// Iterator-protocol guards of array destructuring that are statically false: the
    /// branches they select are not lowered.
    pub(super) iteration: hir_compat::Iteration,
    /// Active tag guards appended as conditions narrow and truncated when branches exit.
    pub(super) variant_guards: Vec<(variants::Path<'s>, &'s str, bool)>,
    pub(super) controls: Vec<Control<'s>>,
    pub(super) inline_returns: Vec<inline::ReturnFrame>,
    pub(super) class_expansions: Vec<crate::classes::ClassExpansion<'s>>,
    pub(super) handlers: Vec<Handler>,
    pub(super) finallies: Vec<Finally<'s>>,
    pub(super) signatures: &'s HashMap<FuncId, Signature<'s>>,
    pub(super) shapes: &'s Shapes<'s>,
    /// Imported built-ins by local name.
    pub(super) builtins: &'s HashMap<&'s str, Builtin>,
    pub(super) result: Option<BindingType>,
    pub(super) is_async: bool,
    pub(super) labels: usize,
    pub(super) rest_excluded_keys: &'s [String],
    pub(super) this_slot: Option<Value>,
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn lower(
        ctx: &mut Context,
        function: &'s Function,
        signatures: &'s HashMap<FuncId, Signature<'s>>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<&'s str, Builtin>,
    ) -> Result<FuncOp> {
        ensure!(
            function
                .captures
                .iter()
                .all(|id| shapes.globals.borrow().contains_key(id)),
            "closures capturing activation-local values are unsupported: {}",
            function.name
        );
        let signature = signatures
            .get(&function.id)
            .with_context(|| format!("no signature for function {}", function.name))?;
        Self::lower_body(ctx, signature, &function.body, signatures, shapes, builtins)
    }

    pub(super) fn lower_body(
        ctx: &mut Context,
        signature: &Signature<'s>,
        body: &'s [Stmt],
        signatures: &'s HashMap<FuncId, Signature<'s>>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<&'s str, Builtin>,
    ) -> Result<FuncOp> {
        let ty = FunctionType::get(
            ctx,
            signature.params.iter().map(|p| p.binding.carrier).collect(),
            signature.carrier_results(),
        );
        let func = FuncOp::new(ctx, signature.name.clone(), ty);
        let mut builder = Self::new(
            ctx,
            &func,
            signature.result,
            signature.is_async,
            signatures,
            shapes,
            builtins,
        );
        let mut globals: Vec<_> = shapes
            .globals
            .borrow()
            .iter()
            .map(|(&id, &binding)| (id, binding))
            .collect();
        globals.sort_by_key(|(id, _)| *id);
        for (id, binding) in globals {
            let slot = builder.global_slot(ctx, id, binding.carrier)?;
            builder.locals.insert(
                id,
                LocalBinding {
                    slot,
                    integer: binding.integer,
                    is_never: false,
                },
            );
        }
        for (index, param) in signature.params.iter().enumerate() {
            let arg_val = builder.entry.deref(ctx).get_argument(index);
            let binding = if param
                .source
                .default
                .as_ref()
                .is_some_and(|expr| !matches!(expr, Expr::Undefined))
            {
                param.base
            } else {
                param.binding
            };
            builder.bind_parameter(ctx, param.source, Some(arg_val), Some(binding))?;
        }
        if signature.await_initialization {
            let promise = builder.intrinsic(ctx, Intrinsic::ModuleInitialize, vec![]);
            let await_ = AwaitOp::new(ctx, promise)?;
            builder.append_value(ctx, &await_);
        }
        if matches!(signature.name.as_ref(), perry_mir::INITIALIZER | MAIN)
            && !shapes.static_fields.is_empty()
        {
            let mut static_inits: Vec<_> = shapes.static_fields.values().copied().collect();
            static_inits.sort_by_key(|(id, _)| *id);
            for (id, field) in static_inits {
                builder.binding(ctx, id, &field.ty, field.init.as_ref())?;
            }
        }
        let body = hir_compat::strip_synthetic_default_guards(
            body,
            signature.params.iter().map(|p| p.source),
        );
        builder.statements(ctx, body)?;
        if !builder.terminated(ctx) {
            let values = builder.implicit_results(ctx);
            builder.emit(ctx, |ctx| ReturnOp::new(ctx, values));
        }
        remove_unreachable(ctx, builder.region);
        Ok(func)
    }

    pub(super) fn new(
        ctx: &Context,
        func: &FuncOp,
        result: Option<BindingType>,
        is_async: bool,
        signatures: &'s HashMap<FuncId, Signature<'s>>,
        shapes: &'s Shapes<'s>,
        builtins: &'s HashMap<&'s str, Builtin>,
    ) -> Self {
        let entry = func.get_entry_block(ctx);
        let region = func.get_operation().deref(ctx).get_region(0);
        Self {
            region,
            entry,
            ins: IRInserter::new_at_block_end(entry),
            block: entry,
            locals: bindings::Bindings::default(),
            closures: bindings::Bindings::default(),
            never_values: HashSet::new(),
            global_slots: HashSet::new(),
            iteration: hir_compat::Iteration::default(),
            variant_guards: Vec::new(),
            controls: Vec::new(),
            inline_returns: Vec::new(),
            class_expansions: Vec::new(),
            handlers: Vec::new(),
            finallies: Vec::new(),
            signatures,
            shapes,
            builtins,
            result,
            is_async,
            labels: 0,
            rest_excluded_keys: &[],
            this_slot: None,
        }
    }
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn wrap_integer(
        &mut self,
        ctx: &mut Context,
        value: Value,
        int_ty: TypedHandle<IntegerType>,
        carrier_ty: TypeHandle,
    ) -> Value {
        let int_handle: TypeHandle = int_ty.into();
        let wrapped = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, int_handle));
        self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, wrapped, carrier_ty))
    }

    pub(super) fn fit_binding(
        &mut self,
        ctx: &mut Context,
        value: Value,
        binding: BindingType,
    ) -> Result<Value> {
        match binding.integer {
            Some(int_ty) if int_ty.deref(ctx).width() == 64 => {
                let int_val = self.fit(ctx, value, int_ty.into())?;
                self.fit(ctx, int_val, binding.carrier)
            }
            Some(int_ty) => {
                let fitted = self.fit(ctx, value, binding.carrier)?;
                Ok(self.wrap_integer(ctx, fitted, int_ty, binding.carrier))
            }
            None => self.fit(ctx, value, binding.carrier),
        }
    }

    /// Results of falling off the end or `return;`: an async function settles `undefined`.
    pub(super) fn implicit_results(&mut self, ctx: &mut Context) -> Vec<Value> {
        self.result
            .into_iter()
            .map(|r| {
                let ty = r.carrier;
                match optional_value(ctx, ty) {
                    Some(_) if admits_undefined(ctx, ty) => {
                        self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
                    }
                    _ => self.emit_value(ctx, |ctx| UndefOp::new(ctx, ty)),
                }
            })
            .collect()
    }

    // ── IR plumbing ────────────────────────────────────────────────────────

    pub(super) fn append(&mut self, ctx: &mut Context, op: &dyn Op) {
        self.ins.append_op(ctx, op);
        self.guard(ctx, op.get_operation());
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

    /// A new block named `label` and a counter. Labels only make printed MIR readable, so the
    /// name is legalised: every label names a block, and an identifier label stays as written.
    pub(super) fn new_block(&mut self, ctx: &mut Context, label: &str) -> Ptr<BasicBlock> {
        self.labels += 1;
        let name = Identifier::try_new(format!("{label}{}", self.labels))
            .unwrap_or_else(|_| Legaliser::default().legalise(&format!("{label}{}", self.labels)));
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

    /// After `return`/`break`/`continue`, keep lowering into an unreachable block.
    pub(super) fn after_terminator(&mut self, ctx: &mut Context) {
        let dead = self.new_block(ctx, "unreachable");
        self.switch_to(dead);
    }

    pub(super) fn load(&mut self, ctx: &mut Context, slot: Value) -> Result<Value> {
        if self.global_slots.contains(&slot) {
            let op = perry_mir::GlobalLoadOp::new(ctx, slot)?;
            return Ok(self.append_value(ctx, &op));
        }
        let op = LoadOp::new(ctx, slot)?;
        Ok(self.append_value(ctx, &op))
    }

    pub(super) fn local_binding(&self, id: LocalId) -> Result<LocalBinding> {
        self.locals.get(&id).copied().ok_or_else(|| {
            anyhow!(
                "unsupported reference to local {id} (globals and captures are not supported yet)"
            )
        })
    }

    pub(super) fn local(&self, id: LocalId) -> Result<Value> {
        Ok(self.local_binding(id)?.slot)
    }

    pub(super) fn integer64_source(&self, ctx: &Context, value: Value) -> Option<Value> {
        let def_op = value.defining_op()?;
        Operation::get_op::<NumericCastOp>(def_op, ctx)?;
        let src = def_op.deref(ctx).get_operand(0);
        is_integer64(ctx, src.get_type(ctx)).then_some(src)
    }

    pub(super) fn load_local(&mut self, ctx: &mut Context, id: LocalId) -> Result<Value> {
        let binding = self.local_binding(id)?;
        let loaded = self.load(ctx, binding.slot)?;
        let result = if let Some(int_ty) = binding.integer
            && int_ty.deref(ctx).width() == 64
        {
            self.wrap_integer(ctx, loaded, int_ty, loaded.get_type(ctx))
        } else {
            loaded
        };
        if binding.is_never {
            self.never_values.insert(result);
        }
        Ok(result)
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

    /// The region's entry block, cached at construction.
    pub(super) fn entry_block(&self) -> Ptr<BasicBlock> {
        self.entry
    }

    /// The slot of module binding `id`, shared by every function that reads or writes it.
    pub(super) fn global_slot(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        ty: TypeHandle,
    ) -> Result<Value> {
        let name = identifier(&format!("global_{id}"))?;
        let slot = self.emit_value(ctx, |ctx| perry_mir::GlobalOp::new(ctx, name, ty));
        self.global_slots.insert(slot);
        Ok(slot)
    }

    pub(super) fn slot_type(&self, ctx: &Context, slot: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<perry_mir::SlotType>::from_handle(slot.get_type(ctx), ctx)
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
}

/// Remove blocks unreachable from the entry block (such as code following `return` or
/// unused exception handlers) so value dominance remains well-formed.
pub(super) fn remove_unreachable(ctx: &mut Context, region: Ptr<pliron::region::Region>) {
    let blocks: Vec<_> = region.deref(ctx).iter(ctx).collect();
    let Some(&entry) = blocks.first() else {
        return;
    };
    let mut reached = HashSet::from([entry]);
    let mut stack = vec![entry];
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

/// Insert `operations`, in order, before the terminator of an already closed `block`: for
/// values whose type is known only after later code was lowered.
pub(super) fn insert_before_terminator(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    operations: impl IntoIterator<Item = Ptr<Operation>>,
) -> Result<()> {
    let terminator = block
        .deref(ctx)
        .get_terminator(ctx)
        .context("a closed block ends in a terminator")?;
    for operation in operations {
        operation.insert_before(ctx, terminator);
    }
    Ok(())
}

impl FunctionBuilder<'_> {
    pub(super) fn emit<T: Op>(
        &mut self,
        ctx: &mut Context,
        make: impl FnOnce(&mut Context) -> T,
    ) -> T {
        let op = make(ctx);
        self.append(ctx, &op);
        op
    }
    pub(super) fn append_value<T: Op + OneResultInterface>(
        &mut self,
        ctx: &mut Context,
        op: &T,
    ) -> Value {
        self.append(ctx, op);
        op.get_result(ctx)
    }
    pub(super) fn emit_value<T: Op + OneResultInterface>(
        &mut self,
        ctx: &mut Context,
        make: impl FnOnce(&mut Context) -> T,
    ) -> Value {
        self.emit(ctx, make).get_result(ctx)
    }
    pub(super) fn entry_slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        let op = LocalOp::new(ctx, ty);
        op.get_operation().insert_at_front(self.entry_block(), ctx);
        op.get_result(ctx)
    }
    pub(super) fn store(&mut self, ctx: &mut Context, slot: Value, value: Value) {
        self.emit(ctx, |ctx| StoreOp::new(ctx, slot, value));
    }
    pub(super) fn jump(&mut self, ctx: &mut Context, target: Ptr<BasicBlock>) {
        if self.block.deref(ctx).get_terminator(ctx).is_none() {
            self.emit(ctx, |ctx| BrOp::new(ctx, target, vec![]));
        }
    }
    pub(super) fn branch(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        yes: Ptr<BasicBlock>,
        no: Ptr<BasicBlock>,
    ) {
        self.emit(ctx, |ctx| {
            CondBrOp::new(ctx, condition, yes, vec![], no, vec![])
        });
    }
    pub(super) fn boolean(&mut self, ctx: &mut Context, value: bool) -> Value {
        let ty = IntegerType::get(ctx, 1, Signedness::Signless);
        self.emit_value(ctx, |ctx| {
            ConstantOp::new(
                ctx,
                Box::new(IntegerAttr::new(
                    ty,
                    APInt::from_u64(u64::from(value), bw(1)),
                )),
            )
        })
    }

    pub(super) fn number(&mut self, ctx: &mut Context, value: f64) -> Value {
        self.emit_value(ctx, |ctx| {
            ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(value)))
        })
    }
    pub(super) fn string(&mut self, ctx: &mut Context, text: &str) -> Value {
        self.emit_value(ctx, |ctx| StringConstOp::new(ctx, text))
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
    pub(super) fn intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Value {
        self.emit(ctx, |ctx| IntrinsicOp::new(ctx, intrinsic, args))
            .get_operation()
            .deref(ctx)
            .get_result(0)
    }
    pub(super) fn intrinsic_void(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) {
        self.emit(ctx, |ctx| IntrinsicOp::new(ctx, intrinsic, args));
    }

    pub(super) fn choose(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        condition: Value,
        then: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        otherwise: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
    ) -> Result<Value> {
        let then_block = self.new_block(ctx, "then");
        let else_block = self.new_block(ctx, "else");
        self.branch(ctx, condition, then_block, else_block);
        self.switch_to(then_block);
        let then_value = then(self, ctx)?;
        let then_end = self.block;
        self.switch_to(else_block);
        let else_value = otherwise(self, ctx)?;
        let [value] = self.merge_branches(
            ctx,
            [ty],
            [(then_end, [then_value]), (self.block, [else_value])],
        )?;
        Ok(value)
    }
    pub(super) fn merge_branches<const N: usize>(
        &mut self,
        ctx: &mut Context,
        types: [TypeHandle; N],
        branches: [(Ptr<BasicBlock>, [Value; N]); 2],
    ) -> Result<[Value; N]> {
        let slots = types.map(|ty| self.entry_slot(ctx, ty));
        let join = self.new_block(ctx, "join");
        for (block, values) in branches {
            self.switch_to(block);
            if self.block.deref(ctx).get_terminator(ctx).is_some() {
                continue;
            }
            for ((slot, ty), value) in slots.into_iter().zip(types).zip(values) {
                let value = self.fit(ctx, value, ty)?;
                self.store(ctx, slot, value);
            }
            self.jump(ctx, join);
        }
        self.switch_to(join);
        let mut values = slots;
        for value in &mut values {
            *value = self.load(ctx, *value)?;
        }
        Ok(values)
    }
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
}
