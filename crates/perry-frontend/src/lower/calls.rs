//! Calls: module functions, WIT imports, imported built-ins and `Promise` statics.
use super::*;
use crate::lower::{builder::LocalBinding, hir_compat::RuntimeCall};

impl<'s> FunctionBuilder<'s> {
    pub(super) fn signature(&self, id: FuncId) -> Result<&'s Signature<'s>> {
        self.signatures
            .get(&id)
            .ok_or_else(|| self.shapes.metadata.locations.missing_function(id))
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
                let absent = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
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
            (None, None) => {
                self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, binding.carrier))
            }
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
        if self
            .shapes
            .specialization
            .instances
            .receiver_parameters
            .contains(&param.id)
        {
            self.this_slot = Some(slot);
        }
        if let Some(owner) = self.local_owner
            && let Some(&ty) = self
                .shapes
                .specialization
                .instances
                .boxed_parameters
                .get(&(owner, param.id))
        {
            let (record, payload) = self.shapes.box_layout(ctx, ty, false)?;
            let value = self.load(ctx, slot)?;
            let value = self.fit(ctx, value, payload)?;
            let value = self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, vec![value]));
            let binding = self.initialized_local(ctx, value);
            self.locals
                .insert(param.id, binding.captured(payload, false));
        }
        if let Some(captures) = self.shapes.specialization.instances.captures.get(&param.id) {
            let environment = self.load(ctx, slot)?;
            for capture in captures {
                let value = self.record_field(ctx, environment, &capture.field)?;
                let mut binding = self.initialized_local(ctx, value);
                if let crate::engine::Storage::Boxed { ty, init } = capture.storage {
                    let deferred = init.admits_undefined();
                    let (_, payload) = self.shapes.box_layout(ctx, ty, deferred)?;
                    binding = binding.captured(payload, deferred);
                }
                self.locals.insert(capture.local, binding);
            }
        }
        Ok(())
    }
    pub(super) fn call(
        &mut self,
        ctx: &mut Context,
        callee: &'s Expr,
        args: &'s [Expr],
        span: std::ops::Range<u32>,
        expected: Option<TypeHandle>,
    ) -> Result<Option<Value>> {
        let byte_offset = span.start;
        if let Expr::IndexGet { object, .. } = callee
            && self
                .shapes
                .metadata
                .locations
                .calls_dispose(span.start, span.end)
        {
            let receiver = self.expression(ctx, object)?;
            let receiver = self.defined(ctx, receiver)?;
            if let Some(resource) =
                perry_mir::typed::<perry_mir::ResourceType>(ctx, receiver.get_type(ctx))
            {
                let owner = resource.deref(ctx).key();
                if let Some(member) =
                    self.shapes
                        .decls
                        .world
                        .member(owner, crate::wit::MemberKind::Dispose, "")
                {
                    return self.resource_call(ctx, member, Some(receiver), args);
                }
            }
            bail_unsupported!("computed method or element calls are not supported yet");
        }
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
            && let Expr::PropertyGet {
                object: namespace,
                property: class,
                ..
            } = &**object
            && let Expr::PropertyGet {
                object: global,
                property: name,
                ..
            } = &**namespace
            && matches!(**global, Expr::GlobalGet(_))
            && name == "Temporal"
        {
            return self.temporal_factory(ctx, class, property, args).map(Some);
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
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            if let Expr::PropertyGet {
                object: global,
                property: class,
                ..
            } = &**object
                && matches!(**global, Expr::GlobalGet(_))
            {
                if class == "performance" {
                    return match property.as_str() {
                        "now" => {
                            for arg in args {
                                self.expression(ctx, arg)?;
                            }
                            Ok(Some(self.intrinsic(ctx, Intrinsic::PerformanceNow, vec![])))
                        }
                        other => bail_unsupported!("performance.{other} is not supported yet"),
                    };
                }
                if class == "Promise" {
                    if property == "resolve" && args.is_empty() {
                        let undef = self.emit_value(ctx, |ctx| {
                            UndefinedOp::new(ctx, FP64Type::get(ctx).into())
                        });
                        return Ok(Some(
                            self.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, undef)),
                        ));
                    }
                    if let ("resolve" | "reject", [value]) = (property.as_str(), args) {
                        return self.promise_settled(ctx, property, value, None).map(Some);
                    }
                    let combinator = match property.as_str() {
                        "all" => Combinator::All,
                        "race" => Combinator::Race,
                        "allSettled" => Combinator::AllSettled,
                        other => bail_unsupported!("Promise.{other} is not supported yet"),
                    };
                    let [inputs] = args else {
                        bail_diag!(codes::TYPE_MISMATCH, "Promise.{property} takes one array");
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
                    let op =
                        PromiseCombinatorOp::new(ctx, combinator, inputs).ok_or_else(|| {
                            Diagnostic::unsupported(
                                "Promise combinators take an array of promises of one type",
                            )
                        })?;
                    return Ok(Some(self.append_value(ctx, &op)));
                }
            }
            return self.method(ctx, object, property, args);
        }
        if hir_compat::is_import_initializer(callee) {
            // Module initialization of a named built-in import (`Writable`): nothing to run.
            return Ok(None);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && let Some(builtin) = self.builtins.get(name.as_str())
        {
            return match builtin {
                Builtin::SetTimeout => self.timer(ctx, args).map(Some),
                Builtin::Writable => {
                    bail_unsupported!("Writable is a class; only Writable.toWeb is supported")
                }
                Builtin::Serve(intrinsic) => {
                    let mut operands = Vec::with_capacity(args.len());
                    for arg in args {
                        let value = self.expression(ctx, arg)?;
                        operands.push(self.defined(ctx, value)?);
                    }
                    Ok(Some(match self.call_intrinsic(ctx, *intrinsic, operands) {
                        Some(value) => value,
                        None => {
                            let number = FP64Type::get(ctx).into();
                            self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number))
                        }
                    }))
                }
                fs => self.fs(ctx, *fs, args).map(Some),
            };
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && let Some((binding, signature)) = self.shapes.foreign_function(name)
        {
            return self.import_call(ctx, binding, signature, args, foreign::CallKind::Function);
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
        let callee = self.shapes.callable(callee);
        if let Expr::Closure { return_type, .. } = callee {
            let prepared = self.prepare_callback(ctx, callee)?;
            let mut arg_vals = Vec::with_capacity(args.len());
            for arg in args {
                arg_vals.push(self.expression(ctx, arg)?);
            }
            if self
                .shapes
                .source_closure_result(ctx, return_type)?
                .is_none()
            {
                self.effect_callback(ctx, prepared, &arg_vals)?;
                return Ok(None);
            }
            return self.array_callback(ctx, prepared, &arg_vals).map(Some);
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
        if let Some(operation) = self.shapes.specialization.instances.operations.get(id) {
            return match operation {
                crate::engine::NativeOperation::Callback(_) => {
                    bail_internal!("expected a callback invocation")
                }
                crate::engine::NativeOperation::Expression(op) => {
                    self.solved_expression(ctx, *op, args, expected)
                }
                crate::engine::NativeOperation::Callable(op) => {
                    self.callable_operation(ctx, *op, args).map(Some)
                }
            };
        }
        let signature = self.signature(*id)?;
        let callee_name = self
            .shapes
            .metadata
            .locations
            .display_function_name(signature.name.as_ref());
        let (fixed, rest) = signature.parameters();
        ensure_diag!(
            rest.is_some() || args.len() <= fixed.len(),
            codes::TYPE_MISMATCH,
            "call to {callee_name} with {} arguments, expected {}",
            args.len(),
            fixed.len()
        );
        let mut values = Vec::with_capacity(signature.params.len());
        for (i, param) in fixed.iter().enumerate() {
            values.push(
                self.bind_fixed_arg(ctx, param, args.get(i))
                    .map_err(|err| {
                        err.with_note(format!("in argument {} of '{callee_name}'", i + 1))
                    })?,
            );
        }
        if let Some(rest) = rest {
            let element = Self::rest_element_type(ctx, rest.binding.carrier)?;
            let mut items = Vec::with_capacity(args.len().saturating_sub(fixed.len()));
            for (offset, arg) in args.iter().skip(fixed.len()).enumerate() {
                let value = self.expression_with(ctx, arg, Some(element))?;
                items.push(self.fit(ctx, value, element).map_err(|err| {
                    err.with_note(format!(
                        "in argument {} of '{callee_name}'",
                        fixed.len() + offset + 1
                    ))
                })?);
            }
            values.push(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, items)));
        }
        self.emit_signature_call(ctx, signature, values)
    }

    fn solved_expression(
        &mut self,
        ctx: &mut Context,
        operation: crate::engine::ExpressionOp,
        args: &'s [Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Option<Value>> {
        use crate::engine::ExpressionOp;
        match (operation, args) {
            (ExpressionOp::Function { identity, target }, args) => {
                let target = self.shapes.resolved_binding(ctx, target)?.carrier;
                let payload = match args {
                    [] => None,
                    [source] => {
                        let value = self.expression(ctx, source)?;
                        let env = self.shapes.environment_type(ctx, identity)?;
                        Some(self.callable_field(ctx, value, "val", env)?)
                    }
                    _ => bail_internal!("invalid function environment"),
                };
                self.function_value(ctx, identity, payload, Some(expected.unwrap_or(target)))
                    .map(Some)
            }
            (
                ExpressionOp::Matches {
                    identity,
                    source,
                    equal,
                },
                [arg],
            ) => self
                .matches_function(ctx, arg, source, identity, equal)
                .map(Some),
            (ExpressionOp::Expected(ty), [value]) => {
                let slot = match expected {
                    Some(slot) if crate::lower::value_unions::is_value_union(ctx, slot) => slot,
                    _ => self.shapes.resolved_binding(ctx, ty)?.carrier,
                };
                self.expression_with(ctx, value, Some(slot)).map(Some)
            }
            (
                ExpressionOp::Promise {
                    settled,
                    executor,
                    handles,
                },
                args,
            ) => self
                .new_promise(ctx, settled, executor, handles, args)
                .map(Some),
            (ExpressionOp::StartTimer(id), args) => self.start_timer(ctx, id, args).map(Some),
            (ExpressionOp::ClearTimer, [handle]) => {
                self.clear_timer(ctx, handle)?;
                Ok(None)
            }
            _ => bail_internal!("invalid operands for specialized expression"),
        }
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

    pub(super) fn bind_fixed_arg(
        &mut self,
        ctx: &mut Context,
        param: &program::ParamSig<'s>,
        arg: Option<&'s Expr>,
    ) -> Result<Value> {
        let carrier = param.binding.carrier;
        if let Some(arg) = arg {
            let expected = (!param.is_never).then_some(carrier);
            let value = self.expression_with(ctx, arg, expected)?;
            return self.fixed_value(ctx, param, value);
        }
        if admits_undefined(ctx, carrier) {
            return Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, carrier)));
        }
        if let Some(default) = param.default {
            let value = self.expression_with(ctx, default, Some(carrier))?;
            return self.fit(ctx, value, carrier);
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
            return Ok(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, carrier)));
        }
        self.fit(ctx, value, carrier)
    }

    pub(super) fn emit_signature_call(
        &mut self,
        ctx: &mut Context,
        signature: &'s Signature<'s>,
        values: Vec<Value>,
    ) -> Result<Option<Value>> {
        if let Some(function) = signature.inline {
            ensure_unsupported!(
                !function.is_async && !self.inline_functions.contains(&function.id),
                "recursive or async calls to capturing function '{}' require a persistent environment",
                function.name
            );
            self.inline_functions.push(function.id);
            let result = self.with_inline_scope(ctx, function.id, self.this_slot, |this, ctx| {
                for ((param, source), value) in
                    signature.params.iter().zip(&function.params).zip(values)
                {
                    this.bind_parameter(ctx, source, Some(value), param.binding, param.is_never)?;
                }
                let result = signature
                    .result
                    .map_or(crate::lower::inline::InlineResult::Void, |ty| {
                        crate::lower::inline::InlineResult::Declared(ty.carrier)
                    });
                let slot = this.inline_body(
                    ctx,
                    this.shapes.specialization.instances.function_body(function),
                    result,
                )?;
                slot.map(|slot| this.load(ctx, slot)).transpose()
            });
            self.inline_functions.pop();
            return result;
        }
        if signature.is_async {
            let result = signature
                .result
                .ok_or_else(|| Diagnostic::internal("async function must settle one result"))?;
            self.invalidate_global_guards();
            return Ok(Some(self.emit_value(ctx, |ctx| {
                AsyncCallOp::new(ctx, signature.name.clone(), values, result.carrier)
            })));
        }
        let op = CallOp::new(
            ctx,
            signature.name.clone(),
            values,
            signature.carrier_results(),
        );
        self.append(ctx, &op);
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
        for &id in &self.shapes.global_ids {
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
            return Ok(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![val])));
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
                let raw = self.field_read(ctx, &op)?;
                let cloned = self.clone_structured_value(ctx, raw)?;
                let cloned_ty = cloned.get_type(ctx);
                let field_val = if can_omit && admits_undefined(ctx, cloned_ty) {
                    let op = FieldPresentOp::new(ctx, val, field_name).to_diag(ctx)?;
                    let is_present = self.emit_value(ctx, |_| op);
                    self.choose(
                        ctx,
                        cloned_ty,
                        is_present,
                        |_, _| Ok(cloned),
                        |b, ctx| Ok(b.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, cloned_ty))),
                    )?
                } else {
                    cloned
                };
                field_vals.push(field_val);
            }
            let allocated = self.emit_value(ctx, |ctx| {
                let alloc = AllocOp::new(ctx, rec, field_vals);
                alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(Vec::new()));
                alloc
            });
            self.intrinsic_void(ctx, Intrinsic::JsonSpread, vec![allocated, val]);
            return Ok(allocated);
        }
        Ok(val)
    }

    /// A combinator's input array with holes read as `undefined` values, so each element is a
    /// promise of `T | undefined`, or of the plain element value.
    fn combinator_holes(&mut self, ctx: &mut Context, inputs: Value) -> Result<Value> {
        use crate::lower::control_flow::{ArrayOutput, Holes, Input};
        let element = self.array_element(ctx, inputs)?;
        let promise = perry_mir::typed::<PromiseType>(ctx, present_type(ctx, element));
        let settled = promise.map(|promise| {
            let value = promise.deref(ctx).value();
            let settled =
                crate::lower::coerce::admitting(ctx, value, perry_mir::Absence::Undefined);
            (settled, PromiseType::get(ctx, settled).into())
        });
        self.collect_items(
            ctx,
            Input::Array(inputs, Holes::Dense),
            |b, ctx, item, _, _| {
                let Some((settled, promise)) = settled else {
                    return Ok(ArrayOutput::Item(
                        b.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, item)),
                    ));
                };
                let hole = b.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, item));
                let promise = b.choose(
                    ctx,
                    promise,
                    hole,
                    |b, ctx| {
                        let value = b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, settled));
                        Ok(b.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, value)))
                    },
                    |b, ctx| {
                        let item = b.defined(ctx, item)?;
                        b.fit(ctx, item, promise)
                    },
                )?;
                Ok(ArrayOutput::Item(promise))
            },
        )
    }

    /// The array literal given to a promise combinator: plain values among promises of `T`
    /// become promises fulfilled with them, as `Promise.all` treats them.
    fn combinator_inputs(&mut self, ctx: &mut Context, items: &'s [Expr]) -> Result<Value> {
        // `Promise.reject(e)` takes its settled type from the other inputs: lower it last.
        let mut inputs = Vec::with_capacity(items.len());
        let mut promised = None;
        let mut first = None;
        for item in items {
            inputs.push(match promise_call(item) {
                Some(("reject", reason)) => CombinatorInput::Reject(reason),
                _ => {
                    let value = self.expression(ctx, item)?;
                    let value = self.defined(ctx, value)?;
                    first.get_or_insert(value);
                    if promised.is_none()
                        && let Some(p) = perry_mir::typed::<PromiseType>(ctx, value.get_type(ctx))
                    {
                        promised = Some(p.deref(ctx).value());
                    }
                    CombinatorInput::Lowered(value)
                }
            });
        }
        let settled = promised
            .or_else(|| first.map(|v| v.get_type(ctx)))
            .unwrap_or_else(|| void_type(ctx));
        let promise: TypeHandle = PromiseType::get(ctx, settled).into();
        let mut values = Vec::with_capacity(items.len());
        for input in inputs {
            let value = match input {
                CombinatorInput::Lowered(value) => value,
                CombinatorInput::Reject(reason) => {
                    self.promise_settled(ctx, "reject", reason, Some(settled))?
                }
            };
            let value = if value.get_type(ctx) == promise {
                value
            } else {
                let value = self.fit(ctx, value, settled)?;
                self.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, value))
            };
            values.push(value);
        }
        Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, promise, values)))
    }

    /// `Promise.resolve(value)` (a promise passes through) or `Promise.reject(reason)`, a
    /// promise of `settled` (`undefined` when nothing says).
    fn promise_settled(
        &mut self,
        ctx: &mut Context,
        name: &str,
        value: &'s Expr,
        settled: Option<TypeHandle>,
    ) -> Result<Value> {
        let value = self.expression(ctx, value)?;
        let value = self.defined(ctx, value)?;
        if name == "resolve" {
            if perry_mir::typed::<PromiseType>(ctx, value.get_type(ctx)).is_some() {
                return Ok(value);
            }
            return Ok(self.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, value)));
        }
        let settled = match settled {
            Some(settled) => settled,
            None => void_type(ctx),
        };
        Ok(self.emit_value(ctx, |ctx| PromiseRejectedOp::new(ctx, value, settled)))
    }
}

/// A promise combinator input: lowered in order, or a `Promise.reject(reason)` lowered once
/// the other inputs decide its settled type.
enum CombinatorInput<'e> {
    Lowered(Value),
    Reject(&'e Expr),
}

/// `Promise.<name>(argument)`: the name and the argument.
fn promise_call(expr: &Expr) -> Option<(&str, &Expr)> {
    let Expr::Call { callee, args, .. } = expr else {
        return None;
    };
    let Expr::PropertyGet {
        object, property, ..
    } = &**callee
    else {
        return None;
    };
    let Expr::PropertyGet {
        object: global,
        property: class,
        ..
    } = &**object
    else {
        return None;
    };
    let [argument] = args.as_slice() else {
        return None;
    };
    (matches!(**global, Expr::GlobalGet(_)) && class == "Promise")
        .then_some((property.as_str(), argument))
}
