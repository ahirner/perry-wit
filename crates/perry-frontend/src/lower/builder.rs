//! The per-function MIR builder: its state and the IR plumbing every lowering uses.
use super::*;
use perry_mir::{IntrinsicOp, LocalOp, StringConstOp};
use pliron::builtin::attributes::FPDoubleAttr;

/// Status of `process.exit` (`ErrorKind::Exit`): handlers pass it on.
pub(super) const EXIT_STATUS: u64 = 5;

#[derive(Clone, Copy)]
pub(super) enum CellRead {
    Value,
    AssertPresent,
}

#[derive(Clone, Copy)]
pub(super) struct LocalBinding<S = Value> {
    pub(super) slot: S,
    pub(super) writes: BindingType,
    pub(super) is_never: bool,
    pub(super) cell: Option<CellRead>,
}

impl<S> LocalBinding<S> {
    pub(super) fn slot(slot: S, writes: BindingType, is_never: bool) -> Self {
        Self {
            slot,
            writes,
            is_never,
            cell: None,
        }
    }
    fn at(self, slot: Value) -> LocalBinding {
        LocalBinding {
            slot,
            writes: self.writes,
            is_never: self.is_never,
            cell: self.cell,
        }
    }
    pub(super) fn captured(self, payload: TypeHandle, deferred: bool) -> Self {
        Self {
            writes: BindingType::plain(payload),
            cell: Some(if deferred {
                CellRead::AssertPresent
            } else {
                CellRead::Value
            }),
            ..self
        }
    }
}

pub(super) struct FunctionBuilder<'s, 'm> {
    pub(super) region: Ptr<pliron::region::Region>,
    /// The region's entry block, where slots are allocated so they dominate every use.
    entry: Ptr<BasicBlock>,
    pub(super) ins: IRInserter<DummyListener>,
    pub(super) block: Ptr<BasicBlock>,
    pub(super) frame: frame::Frame<'s>,
    pub(super) handler: Option<Handler>,
    pub(super) locals: bindings::Bindings<LocalBinding>,
    pub(super) variant_guards: crate::narrowing::Guards<'s>,
    pub(super) never_values: HashSet<Value>,
    pub(super) global_slots: HashSet<Value>,
    pub(super) inline_functions: Vec<FuncId>,
    pub(super) signatures: &'s Functions<'s>,
    pub(super) shapes: &'m mut Shapes<'s>,
    pub(super) result: Option<BindingType>,
    pub(super) is_async: bool,
    pub(super) labels: usize,
}

impl<'s, 'm> FunctionBuilder<'s, 'm> {
    pub(super) fn lower_body(
        ctx: &mut Context,
        signature: &Signature<'s>,
        signatures: &'s Functions<'s>,
        shapes: &'m mut Shapes<'s>,
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
        );
        let globals: Vec<_> = builder
            .shapes
            .globals
            .iter()
            .filter_map(|(&id, &binding)| Some((id, binding?)))
            .collect();
        for (id, layout) in globals {
            let slot = builder.global_slot(ctx, id, layout.slot)?;
            builder.locals.insert(id, layout.at(slot));
        }
        let statements = match signature.body {
            Body::Initializer(statements) => statements,
            Body::Command => &[][..],
            Body::Source(function) | Body::Inline(function) => {
                builder.frame.owner = BodyOwner::Function(function.id);
                builder.shapes.specialization.templates[function.template].body
            }
            Body::Constructor(class, function) => {
                let inputs = builder.entry.deref(ctx).arguments().collect::<Vec<_>>();
                let instance = builder.construct(
                    ctx,
                    class,
                    class.direct_constructor.then_some(inputs.as_slice()),
                )?;
                if !class.direct_constructor {
                    builder.frame.owner = BodyOwner::Function(function.id);
                    for (param, value) in signature.params.iter().zip(inputs) {
                        builder.bind_argument(ctx, param, value)?;
                    }
                    builder.inline_body(
                        ctx,
                        builder.shapes.specialization.templates[function.template].body,
                        super::inline::InlineResult::Void,
                    )?;
                }
                builder.emit(ReturnOp::new(ctx, vec![instance]), ctx);
                remove_unreachable(ctx, builder.region);
                return Ok(func);
            }
            Body::Native(body) => {
                use crate::engine::{NativeBody, solve::HelperSpec};
                let args = builder.entry.deref(ctx).arguments().collect::<Vec<_>>();
                match body {
                    NativeBody::Resource(target) => builder.resource_adapter(ctx, *target, args)?,
                    NativeBody::Helper(HelperSpec::Dispatcher { recv_ty, arms, .. }) => {
                        builder.dispatch_body(ctx, recv_ty.is_some(), *arms, &args)?
                    }
                    NativeBody::Helper(HelperSpec::PromiseReaction(reaction)) => {
                        builder.reaction_body(ctx, *reaction, &args)?
                    }
                    NativeBody::Helper(HelperSpec::TimerRunner {
                        schedule, cb_call, ..
                    }) => builder.timer_body(ctx, *schedule, *cb_call, &args)?,
                    NativeBody::Settle(kind) => builder.settle_body(ctx, *kind, &args)?,
                }
                remove_unreachable(ctx, builder.region);
                return Ok(func);
            }
        };
        for (index, param) in signature.params.iter().enumerate() {
            let value = builder.entry.deref(ctx).get_argument(index);
            builder.bind_argument(ctx, param, value)?;
        }
        if signature.await_initialization {
            let promise = builder.intrinsic(ctx, Intrinsic::ModuleInitialize, vec![]);
            let await_ = AwaitOp::new(ctx, promise).to_diag(ctx)?;
            builder.emit_value(await_, ctx);
        }
        if matches!(signature.body, Body::Initializer(_)) {
            builder.initialize_resources(ctx)?;
            for entry in builder.shapes.specialization.bodies.static_fields.values() {
                builder.binding(
                    ctx,
                    entry.id,
                    &entry.field.ty,
                    None,
                    entry.ty.map(crate::engine::Storage::Value),
                )?;
            }
        }
        builder.statements(ctx, statements)?;
        if !builder.terminated(ctx) {
            let values = builder.implicit_results(ctx);
            builder.emit(ReturnOp::new(ctx, values), ctx);
        }
        remove_unreachable(ctx, builder.region);
        Ok(func)
    }

    pub(super) fn new(
        ctx: &Context,
        func: &FuncOp,
        result: Option<BindingType>,
        is_async: bool,
        signatures: &'s Functions<'s>,
        shapes: &'m mut Shapes<'s>,
    ) -> Self {
        let entry = func.get_entry_block(ctx);
        let region = func.get_operation().deref(ctx).get_region(0);
        Self {
            region,
            entry,
            ins: IRInserter::new_at_block_end(entry),
            block: entry,
            frame: frame::Frame::new(BodyOwner::Initializer, None),
            handler: None,
            locals: Default::default(),
            variant_guards: Default::default(),
            never_values: HashSet::new(),
            global_slots: HashSet::new(),
            inline_functions: Vec::new(),
            signatures,
            shapes,
            result,
            is_async,
            labels: 0,
        }
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn wrap_integer(
        &mut self,
        ctx: &mut Context,
        value: Value,
        int_ty: TypedHandle<IntegerType>,
        carrier_ty: TypeHandle,
    ) -> Value {
        let int_handle: TypeHandle = int_ty.into();
        let wrapped = self.emit_value(NumericCastOp::new(ctx, value, int_handle), ctx);
        self.emit_value(NumericCastOp::new(ctx, wrapped, carrier_ty), ctx)
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
                        self.emit_value(UndefinedOp::with_type(ctx, ty), ctx)
                    }
                    _ => self.emit_value(UndefOp::new(ctx, ty), ctx),
                }
            })
            .collect()
    }

    // ── IR plumbing ────────────────────────────────────────────────────────

    pub(super) fn emit<T: Op>(&mut self, op: T, ctx: &mut Context) -> T {
        self.ins.append_op(ctx, &op);
        self.guard(ctx, op.get_operation());
        op
    }

    pub(super) fn emit_value<T: Op + OneResultInterface>(
        &mut self,
        op: T,
        ctx: &mut Context,
    ) -> Value {
        self.emit(op, ctx).get_result(ctx)
    }

    /// Inside a `try`, route the exception of an op that may throw to the handler.
    pub(super) fn guard(&mut self, ctx: &mut Context, op: Ptr<Operation>) {
        let Some(handler) = self.handler else {
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
        let next = self.new_block(ctx, "caught");
        let branch = CondBrOp::new(ctx, failed, handler, vec![status], next, vec![]);
        self.ins.append_op(ctx, &branch);
        self.switch_to(next);
    }

    pub(super) fn i32(&mut self, ctx: &mut Context, value: u64) -> Value {
        self.emit_value(perry_mir::integer_constant(ctx, 32, value), ctx)
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
            let op = perry_mir::GlobalLoadOp::new(ctx, slot).to_diag(ctx)?;
            return Ok(self.emit_value(op, ctx));
        }
        let op = LoadOp::new(ctx, slot).to_diag(ctx)?;
        Ok(self.emit_value(op, ctx))
    }

    pub(super) fn local_binding(&self, id: LocalId) -> Result<LocalBinding> {
        self.locals.get(&id).copied().ok_or_else(|| {
            unsupported!(
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

    pub(super) fn initialized_local(&mut self, ctx: &mut Context, value: Value) -> LocalBinding {
        let ty = value.get_type(ctx);
        let slot = self.entry_slot(ctx, ty);
        self.store(ctx, slot, value);
        LocalBinding::slot(slot, BindingType::plain(ty), false)
    }

    pub(super) fn assign_local(
        &mut self,
        ctx: &mut Context,
        local: LocalBinding,
        value: Value,
    ) -> Result<Value> {
        let result = self.fit_binding(ctx, value, local.writes)?;
        if local.cell.is_some() {
            let record = self.load(ctx, local.slot)?;
            self.emit(FieldSetOp::new(ctx, record, "v".into(), result), ctx);
            return Ok(result);
        }
        let stored = self.fit(ctx, result, self.slot_type(ctx, local.slot)?)?;
        self.store(ctx, local.slot, stored);
        Ok(result)
    }

    pub(super) fn load_local(&mut self, ctx: &mut Context, id: LocalId) -> Result<Value> {
        let binding = self.local_binding(id)?;
        let loaded = self.load(ctx, binding.slot)?;
        let loaded = if let Some(read) = binding.cell {
            let value = self.record_field(ctx, loaded, "v")?;
            if matches!(read, CellRead::AssertPresent) && self.is_optional(ctx, value) {
                self.emit_value(OptionalCastOp::assert_present(ctx, value), ctx)
            } else {
                value
            }
        } else {
            loaded
        };
        let result = if let Some(int_ty) = binding.writes.integer
            && int_ty.deref(ctx).width() == 64
            && binding.writes.carrier == loaded.get_type(ctx)
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
        self.emit_value(MirBinary::new(ctx, kind, lhs, rhs), ctx)
    }

    /// The region's entry block, cached at construction.
    pub(super) fn entry_block(&self) -> Ptr<BasicBlock> {
        self.entry
    }

    pub(super) fn declare_slot(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        layout: LocalBinding<TypeHandle>,
    ) -> Result<Value> {
        let slot = if let Some(global) = self.shapes.globals.get_mut(&id) {
            *global = Some(layout);
            self.global_slot(ctx, id, layout.slot)?
        } else {
            self.entry_slot(ctx, layout.slot)
        };
        self.variant_guards.assign(id);
        self.locals.insert(id, layout.at(slot));
        Ok(slot)
    }

    /// The slot of module binding `id`, shared by every function that reads or writes it.
    fn global_slot(&mut self, ctx: &mut Context, id: LocalId, ty: TypeHandle) -> Result<Value> {
        self.named_global_slot(ctx, format!("global_{id}"), ty)
    }

    pub(super) fn named_global_slot(
        &mut self,
        ctx: &mut Context,
        name: String,
        ty: TypeHandle,
    ) -> Result<Value> {
        let name = identifier(&name)?;
        let slot = self.emit_value(perry_mir::GlobalOp::new(ctx, name, ty), ctx);
        self.global_slots.insert(slot);
        Ok(slot)
    }

    pub(super) fn slot_type(&self, ctx: &Context, slot: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<perry_mir::SlotType>::from_handle(slot.get_type(ctx), ctx)
                .map_err(|_| Diagnostic::internal("not a slot"))?
                .deref(ctx)
                .value(),
        )
    }

    pub(super) fn array_element(&self, ctx: &Context, array: Value) -> Result<TypeHandle> {
        Ok(
            TypedHandle::<ArrayType>::from_handle(array.get_type(ctx), ctx)
                .map_err(|_| Diagnostic::new(codes::TYPE_MISMATCH, "expected an array"))?
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
    use pliron::graph::traversals::region::post_order_by_component;
    let dead: Vec<_> = post_order_by_component(ctx, &region)
        .into_iter()
        .skip(1)
        .flatten()
        .collect();
    for &block in &dead {
        BasicBlock::drop_all_uses(block, ctx);
    }
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
        .ok_or_else(|| Diagnostic::internal("a closed block ends in a terminator"))?;
    for operation in operations {
        operation.insert_before(ctx, terminator);
    }
    Ok(())
}

impl FunctionBuilder<'_, '_> {
    pub(super) fn entry_slot(&mut self, ctx: &mut Context, ty: TypeHandle) -> Value {
        let op = LocalOp::new(ctx, ty);
        op.get_operation().insert_at_front(self.entry_block(), ctx);
        op.get_result(ctx)
    }
    pub(super) fn store(&mut self, ctx: &mut Context, slot: Value, value: Value) {
        self.emit(StoreOp::new(ctx, slot, value), ctx);
    }
    pub(super) fn jump(&mut self, ctx: &mut Context, target: Ptr<BasicBlock>) {
        if self.block.deref(ctx).get_terminator(ctx).is_none() {
            self.emit(BrOp::new(ctx, target, vec![]), ctx);
        }
    }
    pub(super) fn branch(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        yes: Ptr<BasicBlock>,
        no: Ptr<BasicBlock>,
    ) {
        self.emit(CondBrOp::new(ctx, condition, yes, vec![], no, vec![]), ctx);
    }
    pub(super) fn boolean(&mut self, ctx: &mut Context, value: bool) -> Value {
        self.emit_value(perry_mir::integer_constant(ctx, 1, u64::from(value)), ctx)
    }

    pub(super) fn number(&mut self, ctx: &mut Context, value: f64) -> Value {
        self.emit_value(
            ConstantOp::new(ctx, Box::new(FPDoubleAttr::from(value))),
            ctx,
        )
    }
    pub(super) fn string(&mut self, ctx: &mut Context, text: &str) -> Value {
        self.emit_value(StringConstOp::new(ctx, text), ctx)
    }
    pub(super) fn compare(
        &mut self,
        ctx: &mut Context,
        kind: CompareKind,
        lhs: Value,
        rhs: Value,
    ) -> Value {
        self.emit_value(MirCompare::new(ctx, kind, lhs, rhs), ctx)
    }
    pub(super) fn intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Value {
        self.located_intrinsic(ctx, intrinsic, args, None)
    }

    pub(super) fn located_intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
        span: Option<perry_mir::SourceSpan>,
    ) -> Value {
        let op = self.emit(IntrinsicOp::new(ctx, intrinsic, args), ctx);
        if let Some(span) = span {
            perry_mir::set_source_span(ctx, &op, &span);
        }
        op.get_operation().deref(ctx).get_result(0)
    }

    pub(super) fn intrinsic_void(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) {
        self.emit(IntrinsicOp::new(ctx, intrinsic, args), ctx);
    }

    pub(super) fn choose(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        condition: Value,
        then: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        otherwise: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
    ) -> Result<Value> {
        self.choose_with(ctx, condition, then, otherwise, |_, _| ty)
    }

    pub(super) fn result_block(&self, ctx: &mut Context, ty: TypeHandle) -> Ptr<BasicBlock> {
        self.block_with_args(ctx, [ty]).0
    }

    pub(super) fn block_with_args<const N: usize>(
        &self,
        ctx: &mut Context,
        types: [TypeHandle; N],
    ) -> (Ptr<BasicBlock>, [Value; N]) {
        let block = BasicBlock::new(ctx, None, types.into());
        block.insert_at_back(self.region, ctx);
        (
            block,
            std::array::from_fn(|index| block.deref(ctx).get_argument(index)),
        )
    }

    pub(super) fn yield_if<const N: usize>(
        &mut self,
        ctx: &mut Context,
        done: Ptr<BasicBlock>,
        test: Value,
        values: [Value; N],
    ) {
        let next = self.new_block(ctx, "next");
        self.emit(
            CondBrOp::new(ctx, test, done, values.into(), next, vec![]),
            ctx,
        );
        self.switch_to(next);
    }

    pub(super) fn finish_result(
        &mut self,
        ctx: &mut Context,
        done: Ptr<BasicBlock>,
        answer: Value,
    ) -> Value {
        self.emit(BrOp::new(ctx, done, vec![answer]), ctx);
        self.switch_to(done);
        done.deref(ctx).get_argument(0)
    }

    pub(super) fn choose_with(
        &mut self,
        ctx: &mut Context,
        condition: Value,
        then: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        otherwise: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        result_type: impl FnOnce(&mut Context, [Value; 2]) -> TypeHandle,
    ) -> Result<Value> {
        let yes = self.new_block(ctx, "then");
        let no = self.new_block(ctx, "else");
        self.branch(ctx, condition, yes, no);
        self.switch_to(yes);
        let left = then(self, ctx)?;
        let left_end = self.block;
        self.switch_to(no);
        let right = otherwise(self, ctx)?;
        let ty = result_type(ctx, [left, right]);
        let branches = [(left_end, left), (self.block, right)];
        let join = self.result_block(ctx, ty);
        for (block, value) in branches {
            self.switch_to(block);
            if !self.terminated(ctx) {
                let value = self.fit(ctx, value, ty)?;
                self.emit(BrOp::new(ctx, join, vec![value]), ctx);
            }
        }
        self.switch_to(join);
        Ok(join.deref(ctx).get_argument(0))
    }
    pub(super) fn call_intrinsic(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        args: Vec<Value>,
    ) -> Option<Value> {
        let op = self.emit(IntrinsicOp::new(ctx, intrinsic, args), ctx);
        let operation = op.get_operation().deref(ctx);
        (operation.get_num_results() == 1).then(|| operation.get_result(0))
    }
}
