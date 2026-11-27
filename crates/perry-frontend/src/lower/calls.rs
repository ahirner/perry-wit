//! Calls: module functions, WIT imports, imported built-ins and `Promise` statics.
use super::*;
use crate::engine::solve::{CallTarget, FunctionTarget};
use crate::lower::{builder::LocalBinding, hir_compat::RuntimeCall, inline::InlineResult};

enum SpreadInput {
    Tuple(Value, usize),
    Items(Value),
}

pub(super) enum CallArguments<'s, 'v> {
    Expressions(Arguments<'s>),
    Values(&'v [Value]),
}

impl CallTarget {
    pub(super) fn invoke<'s>(
        self,
        b: &mut FunctionBuilder<'s, '_>,
        ctx: &mut Context,
        value: Option<Value>,
        receiver: Option<Value>,
        args: CallArguments<'s, '_>,
    ) -> Result<Option<Value>> {
        let Some(function) = self.callee() else {
            if let CallArguments::Expressions(args) = args {
                for (arg, spread) in args {
                    if spread {
                        b.call_spread_input(ctx, arg, None)?;
                    } else {
                        b.discard_function_operand(ctx, arg)?;
                    }
                }
                return Ok(Some(b.raise_non_callable(ctx)));
            }
            b.raise_error(ctx, ErrorKind::TypeError, "callee is not a function");
            return Ok(None);
        };
        let signature = b.signature(b.shapes.specialization.instances.emitted_callee(function)?)?;
        let callee = function.operand(b, ctx, value)?;
        let receiver = if self.needs_receiver() {
            Some(receiver.ok_or_else(|| internal!("missing call receiver"))?)
        } else {
            None
        };
        let prefix = callee.into_iter().chain(receiver);
        match args {
            CallArguments::Expressions(args) => {
                b.invoke_arguments(ctx, signature, prefix, args, None)
            }
            CallArguments::Values(args) => {
                let supplied = signature.supplied_values(prefix.clone().count(), args);
                b.invoke_arguments(ctx, signature, prefix.chain(supplied), [], None)
            }
        }
    }
}

impl FunctionTarget {
    pub(super) fn operand(
        self,
        b: &mut FunctionBuilder<'_, '_>,
        ctx: &mut Context,
        value: Option<Value>,
    ) -> Result<Option<Value>> {
        match (self, value) {
            (Self::Direct(_), _) => Ok(None),
            (Self::Dispatcher(_), Some(value)) => Ok(Some(value)),
            (Self::Closure(_), Some(value)) => {
                let wrapper = b.defined(ctx, value)?;
                b.record_field(ctx, wrapper, "val").map(Some)
            }
            _ => bail_internal!("missing callable operand"),
        }
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn signature(&self, id: FuncId) -> Result<&'s Signature<'s>> {
        self.signatures
            .get(&id)
            .ok_or_else(|| internal!("unknown function {}", id))
    }

    /// Bind an argument, evaluating its default only when omitted or undefined.
    pub(super) fn bind_parameter(
        &mut self,
        ctx: &mut Context,
        param: &'s perry_hir::ir::Param,
        value: Option<Value>,
        binding: BindingType,
        is_never: bool,
    ) -> Result<()> {
        let slot = self.entry_slot(ctx, binding.carrier);
        let has_default = param
            .default
            .as_ref()
            .is_some_and(|expr| !matches!(expr, Expr::Undefined));
        let mut join = None;
        let value = match (value, &param.default) {
            (Some(value), Some(default))
                if has_default && admits_undefined(ctx, value.get_type(ctx)) =>
            {
                let absent = self.emit_value(IsUndefinedOp::new(ctx, value), ctx);
                let use_default = self.new_block(ctx, "default_then");
                let use_argument = self.new_block(ctx, "default_else");
                let done = self.new_block(ctx, "default_join");
                self.branch(ctx, absent, use_default, use_argument);
                self.switch_to(use_argument);
                let value = self.strip_undefined(ctx, value)?;
                let value = self.fit_binding(ctx, value, binding)?;
                self.store(ctx, slot, value);
                self.jump(ctx, done);
                self.switch_to(use_default);
                join = Some(done);
                self.expression_with(ctx, default, Some(binding.carrier))?
            }
            (Some(value), _) => value,
            (None, Some(default)) => self.expression_with(ctx, default, Some(binding.carrier))?,
            (None, None) => self.emit_value(UndefinedOp::with_type(ctx, binding.carrier), ctx),
        };
        if !self.terminated(ctx) {
            let value = if has_default {
                self.fit_binding(ctx, value, binding)?
            } else {
                self.fit(ctx, value, binding.carrier)?
            };
            self.store(ctx, slot, value);
            if let Some(done) = join {
                self.jump(ctx, done);
            }
        }
        if let Some(done) = join {
            self.switch_to(done);
        }
        self.locals
            .insert(param.id, LocalBinding::slot(slot, binding, is_never));
        if let Some(crate::engine::Storage::Boxed { ty, .. }) = self.local_storage(param.id) {
            let (record, payload) = self.shapes.box_layout(ctx, ty, false)?;
            let value = self.load(ctx, slot)?;
            let value = self.fit(ctx, value, payload)?;
            let value = self.emit_value(AllocOp::new(ctx, record, vec![value]), ctx);
            let binding = self.initialized_local(ctx, value);
            self.locals
                .insert(param.id, binding.captured(payload, false));
        }
        Ok(())
    }

    pub(super) fn bind_argument(
        &mut self,
        ctx: &mut Context,
        param: &program::ParamSig<'s>,
        value: Value,
    ) -> Result<()> {
        match param.origin {
            program::Parameter::Source(_, source) => {
                let binding = if matches!(param.default(), Some((_, expr)) if !matches!(expr, Expr::Undefined))
                {
                    param.base
                } else {
                    param.binding
                };
                return self.bind_parameter(ctx, source, Some(value), binding, param.is_never);
            }
            program::Parameter::Receiver => {
                self.frame.this_slot = Some(self.initialized_local(ctx, value).slot);
            }
            program::Parameter::Environment(identity) => {
                let layout = self.shapes.environment(identity)?;
                for (slot, field) in self.shapes.specialization.instances.environment_fields
                    [layout.fields]
                    .iter()
                    .enumerate()
                {
                    if matches!(field, crate::engine::EnvironmentField::Receiver(_)) {
                        let receiver = self.capture_field(ctx, value, slot)?;
                        self.frame.this_slot = Some(self.initialized_local(ctx, receiver).slot);
                        continue;
                    }
                    let &crate::engine::EnvironmentField::Capture {
                        local, ty, cell, ..
                    } = field
                    else {
                        continue;
                    };
                    let captured = self.capture_field(ctx, value, slot)?;
                    let mut binding = self.initialized_local(ctx, captured);
                    if let Some(init) = cell {
                        let deferred = init.admits_undefined();
                        let (_, payload) = self.shapes.box_layout(ctx, ty, deferred)?;
                        binding = binding.captured(payload, deferred);
                    }
                    self.locals.insert(local, binding);
                }
            }
            program::Parameter::Native => bail_internal!("native argument in source function"),
        }
        Ok(())
    }
    fn call_spread_input(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        element: Option<TypeHandle>,
    ) -> Result<SpreadInput> {
        let source = self.spread_operand(ctx, expr, element)?;
        let source = self.defined(ctx, source)?;
        if let Some(record) = perry_mir::typed::<RecordType>(ctx, source.get_type(ctx))
            && record.deref(ctx).kind() == RecordKind::Tuple
        {
            return Ok(SpreadInput::Tuple(
                source,
                record.deref(ctx).fields().to_diag(ctx)?.len(),
            ));
        }
        Ok(SpreadInput::Items(self.spread_items(ctx, source, element)?))
    }

    pub(super) fn invoke_site(
        &mut self,
        ctx: &mut Context,
        target: CallTarget,
        call: crate::source::SourceCall<'s>,
    ) -> Result<Option<Value>> {
        let args = CallArguments::Expressions(call.args);
        let crate::source::Callee::Value(callee) = call.callee else {
            return target.invoke(self, ctx, None, None, args);
        };
        let mut receiver = None;
        let callable = if let Some((object, key)) = hir_compat::member(callee)
            && target.needs_receiver()
        {
            let value = self.expression(ctx, object)?;
            let get = self.getter(callee)?;
            let callable = if target.needs_value() || get.is_some() {
                self.read_member(ctx, object, value, key, get)?
            } else {
                if let hir_compat::MemberKey::Computed(index, _) = key {
                    self.effect(ctx, index)?;
                }
                None
            };
            receiver = Some(self.defined(ctx, value)?);
            callable
        } else if target.needs_value() {
            Some(self.expression(ctx, callee)?)
        } else {
            self.discard_function_operand(ctx, callee)?;
            None
        };
        target.invoke(self, ctx, callable, receiver, args)
    }

    pub(super) fn invoke_arguments(
        &mut self,
        ctx: &mut Context,
        signature: &'s Signature<'s>,
        implicit: impl Iterator<Item = Value>,
        args: impl IntoIterator<Item = (&'s Expr, bool)>,
        result: Option<InlineResult>,
    ) -> Result<Option<Value>> {
        let values = self.bind_arguments(ctx, signature, 0, implicit, args)?;
        self.emit_signature_call(ctx, signature, values, result)
    }

    pub(super) fn bind_arguments(
        &mut self,
        ctx: &mut Context,
        signature: &'s Signature<'s>,
        reserved: usize,
        implicit: impl Iterator<Item = Value>,
        args: impl IntoIterator<Item = (&'s Expr, bool)>,
    ) -> Result<Vec<Value>> {
        let (fixed, rest) = signature.parameters();
        let rest_element = rest
            .map(|p| Self::rest_element_type(ctx, p.binding.carrier))
            .transpose()?;
        let mut values = Vec::with_capacity(signature.params.len());
        let mut fixed = fixed.iter().skip(reserved).peekable();
        let mut rest_values = Vec::new();
        for value in implicit {
            if let Some(param) = fixed.next() {
                values.push(self.fixed_value(ctx, param, value)?);
            } else if let Some(element) = rest_element {
                rest_values.push(self.fit(ctx, value, element)?);
            }
        }
        let callee_name = self
            .shapes
            .locations
            .display_function_name(signature.name.as_ref());
        let mut argument_index = reserved + values.len();
        let mut rest_array = None;
        for (expr, spread) in args {
            let (tuple, count) = if spread {
                let hint = rest_element.filter(|_| fixed.len() == 0);
                match self.call_spread_input(ctx, expr, hint)? {
                    SpreadInput::Tuple(value, count) => (Some(value), count),
                    SpreadInput::Items(source) => {
                        ensure_unsupported!(
                            fixed.len() == 0,
                            "spreading into fixed parameters requires a tuple"
                        );
                        if let Some(element) = rest_element {
                            let array = *rest_array.get_or_insert_with(|| {
                                self.emit_value(
                                    ArrayNewOp::new(ctx, element, std::mem::take(&mut rest_values)),
                                    ctx,
                                )
                            });
                            let ty = array.get_type(ctx);
                            let source = self.fit(ctx, source, ty)?;
                            self.emit(perry_mir::ArrayExtendOp::new(ctx, array, source), ctx);
                        }
                        argument_index += 1;
                        continue;
                    }
                }
            } else {
                (None, 1)
            };
            for field in 0..count {
                let index = argument_index;
                argument_index += 1;
                let param = fixed.peek().copied();
                if param.or(rest).is_none_or(|next| index < next.position) {
                    if tuple.is_none() {
                        self.discard_function_operand(ctx, expr)?;
                    }
                    continue;
                }
                let hint =
                    param.map_or(rest_element, |p| (!p.is_never).then_some(p.binding.carrier));
                let value = match tuple {
                    Some(source) => self.record_field(ctx, source, &format!("_{field}"))?,
                    None => self.expression_with(ctx, expr, hint)?,
                };
                let note = |error: Diagnostic| {
                    error.with_note(format!("in argument {} of '{callee_name}'", index + 1))
                };
                if let Some(param) = fixed.next() {
                    values.push(self.fixed_value(ctx, param, value).map_err(note)?);
                } else if let Some(element) = rest_element {
                    let value = self.fit(ctx, value, element).map_err(note)?;
                    match rest_array {
                        Some(array) => {
                            self.emit(ArrayPushOp::new(ctx, array, value), ctx);
                        }
                        None => rest_values.push(value),
                    }
                }
            }
        }
        for param in fixed {
            let position = param.position + 1;
            values.push(self.missing_argument(ctx, param).map_err(|error| {
                error.with_note(format!("in argument {position} of '{callee_name}'"))
            })?);
        }
        if let Some(element) = rest_element {
            values.push(rest_array.unwrap_or_else(|| {
                self.emit_value(ArrayNewOp::new(ctx, element, rest_values), ctx)
            }));
        }
        Ok(values)
    }

    pub(super) fn clear_timer_args(
        &mut self,
        ctx: &mut Context,
        has_handle: bool,
        args: &'s [Expr],
    ) -> Result<()> {
        let mut handle = None;
        for (i, arg) in args.iter().enumerate() {
            if i == 0 && has_handle {
                handle = Some(self.expression(ctx, arg)?);
            } else {
                self.discard_function_operand(ctx, arg)?;
            }
        }
        if let Some(handle) = handle {
            self.clear_timer(ctx, handle)?;
        }
        Ok(())
    }

    pub(super) fn call(
        &mut self,
        ctx: &mut Context,
        callee: &'s Expr,
        args: &'s [Expr],
        byte_offset: u32,
    ) -> Result<Option<Value>> {
        if let Some(call) = hir_compat::runtime_call(callee, args) {
            return match call {
                RuntimeCall::Unresolved(ident) => Err(self.shapes.annotate_byte_offset(
                    diag!(codes::TYPE_MISMATCH, "cannot find name '{ident}'"),
                    byte_offset,
                    ident.len() as u32,
                )),
                RuntimeCall::CheckInitialized { local, value } => {
                    let value = value.map(|value| self.expression(ctx, value)).transpose()?;
                    self.load(ctx, self.local(local)?)?;
                    Ok(value)
                }
                RuntimeCall::CollectionEpoch => Ok(Some(self.number(ctx, 0.0))),
                RuntimeCall::CollectionCursor { collection, index } => {
                    let collection = self.expression(ctx, collection)?;
                    let index = self.expression(ctx, index)?;
                    let len = self.array_length(ctx, collection);
                    let in_bounds = self.compare(ctx, CompareKind::Lt, index, len);
                    let absent = self.number(ctx, -1.0);
                    self.choose(
                        ctx,
                        FP64Type::get(ctx).into(),
                        in_bounds,
                        |_, _| Ok(index),
                        |_, _| Ok(absent),
                    )
                    .map(Some)
                }
                RuntimeCall::ExitCode(args) => {
                    let [code] = args else {
                        bail_diag!(codes::TYPE_MISMATCH, "process.exitCode takes one value");
                    };
                    let code = self.expression(ctx, code)?;
                    let code = self.arithmetic(ctx, code);
                    ensure_diag!(
                        kind(ctx, code)? == Kind::Number,
                        codes::TYPE_MISMATCH,
                        "process.exitCode must be a number"
                    );
                    self.intrinsic_void(ctx, Intrinsic::ExitCodeSet, vec![code]);
                    Ok(None)
                }
            };
        }
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
            && matches!(**object, Expr::GlobalGet(0))
        {
            if property == "fetch" {
                return self.fetch(ctx, args).map(Some);
            }
            let intrinsic = match property.as_str() {
                "log" | "info" | "debug" => Intrinsic::PrintOut,
                "error" | "warn" => Intrinsic::PrintErr,
                other => bail_unsupported!("console.{other} is not supported yet"),
            };
            let line = self.console_line(ctx, args)?;
            self.call_intrinsic(ctx, intrinsic, vec![line]);
            return Ok(None);
        }
        if let Some((object, property)) = self.shapes.specialization.receiver_members.method(callee)
        {
            return self.method(ctx, object, property, args, Some(byte_offset));
        }
        if hir_compat::is_import_initializer(callee) {
            // Module initialization of a named built-in import (`Writable`): nothing to run.
            return Ok(None);
        }
        if let Expr::ExternFuncRef { name, .. } = callee {
            match self.shapes.decls.imports.get(name.as_str()).copied() {
                Some(Imported::TimeoutPromise) => return self.timer(ctx, args).map(Some),
                Some(Imported::Writable) => {
                    bail_unsupported!("Writable is a class; only Writable.toWeb is supported")
                }
                Some(Imported::Service(intrinsic)) => {
                    let mut operands = Vec::with_capacity(args.len());
                    for arg in args {
                        let value = self.expression(ctx, arg)?;
                        operands.push(self.defined(ctx, value)?);
                    }
                    return Ok(Some(match self.call_intrinsic(ctx, intrinsic, operands) {
                        Some(value) => value,
                        None => {
                            let number = FP64Type::get(ctx).into();
                            self.emit_value(UndefinedOp::new(ctx, number), ctx)
                        }
                    }));
                }
                Some(Imported::ReadFile) => {
                    let text = hir_compat::read_file_text(
                        args.get(1),
                        &self.shapes.specialization.literal_classes,
                    )?;
                    return self.read_file(ctx, args, text).map(Some);
                }
                Some(Imported::WriteFile) => return self.write_file(ctx, args).map(Some),
                Some(Imported::Path(op, label)) => {
                    let path = self.fs_path(ctx, args)?;
                    ensure_unsupported!(
                        args.len() == 1,
                        "{label} options are not supported yet (recursive, withFileTypes)"
                    );
                    return Ok(Some(self.intrinsic(ctx, op, vec![path])));
                }
                Some(Imported::Foreign(signature)) => {
                    return self.import_call(ctx, signature, args, foreign::CallKind::Function);
                }
                _ if let Some(signature) = self.shapes.decls.world.function(name) => {
                    return self.import_call(ctx, signature, args, foreign::CallKind::Function);
                }
                _ => {}
            }
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && name == "structuredClone"
        {
            let [arg] = args else {
                bail_diag!(
                    codes::TYPE_MISMATCH,
                    "structuredClone requires one argument"
                );
            };
            let val = self.expression(ctx, arg)?;
            return self.clone_structured_value(ctx, val).map(Some);
        }
        if matches!(callee, Expr::Closure { .. })
            || matches!(callee, Expr::FuncRef(id) if self.signatures.get(id).is_none())
        {
            let callback = self.prepare_callback(ctx, callee)?;
            let values = args
                .iter()
                .map(|arg| self.expression(ctx, arg))
                .collect::<Result<Vec<_>>>()?;
            return self.invoke_callback(ctx, callback, &values, None);
        }
        let Expr::FuncRef(id) = callee else {
            return Err(match callee {
                Expr::LocalGet(id) => {
                    let error = unsupported!(
                        "indirect calls through local or parameter bindings are not supported yet; call a declared module function directly"
                    );
                    let error = self.shapes.annotate_call(error, byte_offset);
                    self.shapes.annotate_local(error, *id)
                }
                Expr::ExternFuncRef { name, .. } => {
                    unsupported!(
                        "call to external or unresolved function '{name}' is not supported yet"
                    )
                }
                Expr::IndexGet { .. } => {
                    unsupported!("computed method or element calls are not supported yet")
                }
                _ => unsupported!(
                    "indirect function calls are not supported yet; call a declared module function directly"
                ),
            });
        };
        let signature = self.signature(*id)?;
        self.invoke_arguments(
            ctx,
            signature,
            std::iter::empty(),
            Arguments::Plain(args),
            None,
        )
    }

    pub(super) fn rest_element_type(ctx: &Context, carrier: TypeHandle) -> Result<TypeHandle> {
        let array = TypedHandle::<ArrayType>::from_handle(carrier, ctx).map_err(|_| {
            Diagnostic::new(
                codes::TYPE_MISMATCH,
                "rest parameter must have an array type",
            )
        })?;
        Ok(array.deref(ctx).element())
    }

    fn missing_argument(
        &mut self,
        ctx: &mut Context,
        param: &program::ParamSig<'s>,
    ) -> Result<Value> {
        let carrier = param.binding.carrier;
        if admits_undefined(ctx, carrier) {
            return Ok(self.emit_value(UndefinedOp::with_type(ctx, carrier), ctx));
        }
        if let Some((owner, default)) = param.default() {
            let saved = std::mem::replace(&mut self.frame.owner, BodyOwner::Function(owner));
            let value = self.expression_with(ctx, default, Some(carrier));
            self.frame.owner = saved;
            return self.fit(ctx, value?, carrier);
        }
        bail_diag!(codes::TYPE_MISMATCH, "missing required argument")
    }

    pub(super) fn fixed_value(
        &mut self,
        ctx: &mut Context,
        param: &program::ParamSig<'_>,
        value: Value,
    ) -> Result<Value> {
        let carrier = param.binding.carrier;
        if param.is_never && value.get_type(ctx) != carrier {
            return Ok(self.emit_value(UndefinedOp::with_type(ctx, carrier), ctx));
        }
        self.fit(ctx, value, carrier)
    }

    pub(super) fn emit_signature_call(
        &mut self,
        ctx: &mut Context,
        signature: &'s Signature<'s>,
        values: Vec<Value>,
        expected: Option<InlineResult>,
    ) -> Result<Option<Value>> {
        if let Body::Inline(function) = signature.body {
            ensure_unsupported!(
                !signature.is_async && !self.inline_functions.contains(&function.id),
                "recursive or async calls to capturing function '{}' require a persistent environment",
                function.name
            );
            self.inline_functions.push(function.id);
            let result =
                self.with_inline_scope(ctx, function.id, self.frame.this_slot, |this, ctx| {
                    for (param, value) in signature.params.iter().zip(values) {
                        this.bind_argument(ctx, param, value)?;
                    }
                    let result = expected.unwrap_or_else(|| {
                        signature
                            .result
                            .map_or(InlineResult::Void, |ty| InlineResult::Declared(ty.carrier))
                    });
                    this.inline_body(
                        ctx,
                        this.shapes.specialization.templates[function.template].body,
                        result,
                    )
                });
            self.inline_functions.pop();
            return result;
        }
        if signature.is_async {
            let result = signature
                .result
                .ok_or_else(|| Diagnostic::internal("async function must settle one result"))?;
            self.invalidate_global_guards();
            return Ok(Some(self.emit_value(
                AsyncCallOp::new(ctx, signature.name.clone(), values, result.carrier),
                ctx,
            )));
        }
        let op = CallOp::new(
            ctx,
            signature.name.clone(),
            values,
            signature.carrier_results(),
        );
        let op = self.emit(op, ctx);
        self.invalidate_global_guards();
        let Some(result_ty) = signature.result else {
            return Ok(None);
        };
        let mut value = op.get_operation().deref(ctx).get_result(0);
        if let Some(int_ty) = result_ty.integer {
            value = self.wrap_integer(ctx, value, int_ty, result_ty.carrier);
        }
        if signature.returns_never {
            self.never_values.insert(value);
        }
        Ok(Some(value))
    }

    fn invalidate_global_guards(&mut self) {
        if self.variant_guards.depth() == 0 {
            return;
        }
        for &id in self.shapes.globals.keys() {
            self.variant_guards.assign(id);
        }
    }

    pub(super) fn clone_structured_value(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        let ty = val.get_type(ctx);
        if perry_mir::typed::<ArrayType>(ctx, ty).is_some() {
            return Ok(self.emit_value(ConcatOp::from_sources(ctx, vec![val]), ctx));
        }
        if let Some(rec) = perry_mir::typed::<RecordType>(ctx, ty) {
            let fields = rec
                .deref(ctx)
                .fields()
                .map_err(|_| Diagnostic::internal("record without fields"))?
                .to_vec();
            let mut field_vals = Vec::with_capacity(fields.len());
            for (field_name, field_ty) in fields {
                let can_omit = admits_undefined(ctx, field_ty);
                let op = FieldGetOp::new(ctx, val, field_name.clone()).to_diag(ctx)?;
                let raw = self.field_read(ctx, op)?;
                let cloned = self.clone_structured_value(ctx, raw)?;
                let cloned_ty = cloned.get_type(ctx);
                let field_val = if can_omit && admits_undefined(ctx, cloned_ty) {
                    let op = FieldPresentOp::new(ctx, val, field_name).to_diag(ctx)?;
                    let is_present = self.emit_value(op, ctx);
                    self.choose(
                        ctx,
                        cloned_ty,
                        is_present,
                        |_, _| Ok(cloned),
                        |b, ctx| Ok(b.emit_value(OmittedOp::with_type(ctx, cloned_ty), ctx)),
                    )?
                } else {
                    cloned
                };
                field_vals.push(field_val);
            }
            let allocated =
                self.emit_value(AllocOp::ordered(ctx, rec, field_vals, Vec::new()), ctx);
            self.intrinsic_void(ctx, Intrinsic::JsonSpread, vec![allocated, val]);
            return Ok(allocated);
        }
        Ok(val)
    }

    pub(super) fn static_call(
        &mut self,
        ctx: &mut Context,
        method: crate::methods::StaticMethod,
        args: &'s [Expr],
    ) -> Result<Value> {
        use crate::methods::StaticMethod::*;
        match method {
            PerformanceNow => {
                for arg in args {
                    self.expression(ctx, arg)?;
                }
                Ok(self.intrinsic(ctx, Intrinsic::PerformanceNow, vec![]))
            }
            Resolve if args.is_empty() => {
                let undef = self.emit_value(UndefinedOp::new(ctx, FP64Type::get(ctx).into()), ctx);
                Ok(self.emit_value(PromiseResolvedOp::new(ctx, undef), ctx))
            }
            Resolve | Reject => {
                let [value] = args else {
                    let name = if matches!(method, Resolve) {
                        "resolve"
                    } else {
                        "reject"
                    };
                    bail_unsupported!("Promise.{name} is not supported yet")
                };
                let value = self.expression(ctx, value)?;
                if matches!(method, Resolve) {
                    self.as_promise_value(ctx, value)
                } else {
                    let settled = void_type(ctx);
                    Ok(self.emit_value(PromiseRejectedOp::new(ctx, value, settled), ctx))
                }
            }
            Combine(combinator) => {
                let [inputs] = args else {
                    let name = match combinator {
                        Combinator::All => "all",
                        Combinator::AllSettled => "allSettled",
                        Combinator::Race => "race",
                    };
                    bail_diag!(codes::TYPE_MISMATCH, "Promise.{name} takes one array");
                };
                let inputs = match inputs {
                    Expr::Array(items, _) => self.combinator_inputs(ctx, items)?,
                    inputs => self.expression(ctx, inputs)?,
                };
                let inputs = if self.shapes.specialization.sparse_arrays {
                    self.combinator_holes(ctx, inputs)?
                } else {
                    inputs
                };

                let op = PromiseCombinatorOp::new(ctx, combinator, inputs).ok_or_else(|| {
                    unsupported!("Promise combinators take an array of promises of one type")
                })?;
                Ok(self.emit_value(op, ctx))
            }
            Unsupported(id) => Err(Diagnostic::unsupported(
                self.shapes.types.source.name_str(id).to_owned(),
            )),
            Temporal(factory) => self.temporal_factory(ctx, factory, args),
        }
    }

    /// A combinator's input array with holes read as `undefined` values, so each element is a
    /// promise of `T | undefined`, or of the plain element value.
    fn combinator_holes(&mut self, ctx: &mut Context, inputs: Value) -> Result<Value> {
        use crate::lower::control_flow::{ArrayOutput, Holes, Input};
        self.collect_items(
            ctx,
            Input::Array(inputs, Holes::Dense),
            |b, ctx, item, _, _| Ok(ArrayOutput::Item(b.as_promise_value(ctx, item)?)),
        )
    }

    /// The array literal given to a promise combinator: plain values among promises of `T`
    /// become promises fulfilled with them, as `Promise.all` treats them.
    fn combinator_inputs(&mut self, ctx: &mut Context, items: &'s [Expr]) -> Result<Value> {
        let mut inputs = Vec::with_capacity(items.len());
        let mut promised = None;
        let mut first = None;
        for item in items {
            if matches!(
                self.site(item)?.resolution,
                Some(crate::engine::solve::SiteResolution::Static(
                    crate::methods::StaticMethod::Reject
                ))
            ) && let Some(([reason], _)) = hir_compat::call_arguments(item)
            {
                inputs.push(CombinatorInput::Reject(self.expression(ctx, reason)?));
            } else {
                let value = self.expression(ctx, item)?;
                let ty = value.get_type(ctx);
                first.get_or_insert(ty);
                if let Some(promise) = perry_mir::typed::<PromiseType>(ctx, ty) {
                    promised.get_or_insert(promise.deref(ctx).value());
                }
                inputs.push(CombinatorInput::Lowered(value));
            }
        }
        let settled = promised.or(first).unwrap_or_else(|| void_type(ctx));
        let promise: TypeHandle = PromiseType::get(ctx, settled).into();
        let mut values = Vec::with_capacity(items.len());
        for input in inputs {
            let value = match input {
                CombinatorInput::Lowered(value) => value,
                CombinatorInput::Reject(reason) => {
                    self.emit_value(PromiseRejectedOp::new(ctx, reason, settled), ctx)
                }
            };
            let value = if value.get_type(ctx) == promise {
                value
            } else {
                let value = self.fit(ctx, value, settled)?;
                self.emit_value(PromiseResolvedOp::new(ctx, value), ctx)
            };
            values.push(value);
        }
        Ok(self.emit_value(ArrayNewOp::new(ctx, promise, values), ctx))
    }
}

enum CombinatorInput {
    Lowered(Value),
    Reject(Value),
}
