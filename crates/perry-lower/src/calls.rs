//! Calls: module functions, WIT imports, imported built-ins and `Promise` statics.
use super::*;
use crate::{builder::LocalBinding, coerce::merge_absence, hir_compat::RuntimeCall};
use perry_mir::OptionalType;

impl<'s> FunctionBuilder<'s> {
    /// Bind a supplied argument, evaluating its default only on the undefined path.
    pub(super) fn bind_inline_parameter(
        &mut self,
        ctx: &mut Context,
        param: &'s perry_hir::ir::Param,
        value: Value,
        expected: Option<TypeHandle>,
    ) -> Result<()> {
        let value_type = value.get_type(ctx);
        let slot = if let Some(default) = &param.default
            && !matches!(default, Expr::Undefined)
            && admits_undefined(ctx, value_type)
        {
            let is_undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            let then_block = self.new_block(ctx, "default_then");
            let else_block = self.new_block(ctx, "default_else");
            let join = self.new_block(ctx, "default_join");
            self.branch(ctx, is_undefined, then_block, else_block);

            self.switch_to(else_block);
            let present = self.strip_undefined(ctx, value)?;
            let else_end = self.block;
            let present_type = present.get_type(ctx);

            self.switch_to(then_block);
            let default = self.expression_with(ctx, default, expected.or(Some(present_type)))?;
            let then_end = self.block;
            let terminated = self.terminated(ctx);
            let target = if let Some(expected) = expected {
                expected
            } else if !terminated
                && let Some(states) = merge_absence(
                    absence(ctx, present_type),
                    absence(ctx, default.get_type(ctx)),
                )
            {
                let inner = optional_value(ctx, present_type)
                    .or_else(|| optional_value(ctx, default.get_type(ctx)))
                    .unwrap_or(present_type);
                OptionalType::get_with(ctx, inner, states).into()
            } else {
                present_type
            };

            let slot = self.entry_slot(ctx, target);
            if !terminated {
                self.switch_to(then_end);
                let default = self.fit(ctx, default, target)?;
                self.store(ctx, slot, default);
                self.jump(ctx, join);
            }
            self.switch_to(else_end);
            let present = self.fit(ctx, present, target)?;
            self.store(ctx, slot, present);
            self.jump(ctx, join);
            self.switch_to(join);
            slot
        } else {
            let target = expected.unwrap_or(value_type);
            let value = self.fit(ctx, value, target)?;
            let slot = self.entry_slot(ctx, target);
            self.store(ctx, slot, value);
            slot
        };
        self.locals.insert(param.id, LocalBinding::plain(slot));
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
                    anyhow!("cannot find name '{ident}'"),
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
                        bail!("process.exitCode takes one value");
                    };
                    let code = self.expression(ctx, code)?;
                    let code = self.arithmetic(ctx, code);
                    ensure!(
                        kind(ctx, code)? == Kind::Number,
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
                other => bail!("console.{other} is not supported yet"),
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
                && class == "Promise"
            {
                if let ("resolve" | "reject", [value]) = (property.as_str(), args) {
                    return self.promise_settled(ctx, property, value, None).map(Some);
                }
                let combinator = match property.as_str() {
                    "all" => Combinator::All,
                    "race" => Combinator::Race,
                    "allSettled" => Combinator::AllSettled,
                    other => bail!("Promise.{other} is not supported yet"),
                };
                let [inputs] = args else {
                    bail!("Promise.{property} takes one array");
                };
                let inputs = match inputs {
                    Expr::Array(items) => self.combinator_inputs(ctx, items)?,
                    inputs => self.expression(ctx, inputs)?,
                };
                let op = PromiseCombinatorOp::new(ctx, combinator, inputs)
                    .context("Promise combinators take an array of promises of one type")?;
                return Ok(Some(self.append_value(ctx, &op)));
            }
            return self.method(ctx, object, property, args);
        }
        if hir_compat::is_import_initializer(callee) {
            // Module initialization of a named built-in import (`Writable`): nothing to run.
            return Ok(None);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && let Some(builtin) = self.builtins.get(name)
        {
            return match builtin {
                Builtin::SetTimeout => self.timer(ctx, args).map(Some),
                Builtin::Writable => bail!("Writable is a class; only Writable.toWeb is supported"),
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
            ensure!(
                args.len() == signature.params.len(),
                "WIT import {binding} expects {} arguments",
                signature.params.len()
            );
            let mut values = Vec::new();
            for (arg, (_, ty, _)) in args.iter().zip(&signature.params) {
                let ty = self.shapes.value_type(ctx, ty)?;
                let value = self.expression_with(ctx, arg, Some(ty))?;
                let actual = value.get_type(ctx);
                let value = if foreign::accepts_input(ctx, ty, actual) {
                    value
                } else if optional_value(ctx, ty).is_none()
                    && optional_value(ctx, actual)
                        .is_some_and(|inner| foreign::accepts_input(ctx, ty, inner))
                {
                    self.defined(ctx, value)?
                } else if optional_value(ctx, actual).is_none()
                    && let Some((inner, states)) = optional_parts(ctx, ty)
                    && foreign::accepts_input(ctx, inner, actual)
                {
                    let wrapped = perry_mir::OptionalType::get_with(ctx, actual, states).into();
                    self.emit_value(ctx, |ctx| WrapOp::with_type(ctx, value, wrapped))
                } else {
                    self.fit(ctx, value, ty)?
                };
                values.push(value);
            }
            let results = if *signature.return_type == HirType::Void {
                vec![]
            } else {
                vec![self.shapes.value_type(ctx, &signature.return_type)?]
            };
            if signature.is_async {
                let payload = self.shapes.settled_type(ctx, &signature.return_type)?;
                let params = values.iter().map(|v| v.get_type(ctx)).collect();
                let wrapper = foreign::wrapper(ctx, self.shapes, &binding, params, payload)?;
                let op = AsyncCallOp::new(ctx, wrapper, values, payload);
                return Ok(Some(self.append_value(ctx, &op)));
            }
            let op =
                perry_mir::ForeignOp::new(ctx, perry_mir::Phase::Call, &binding, values, results);
            self.append(ctx, &op);
            return Ok(op.get_operation().deref(ctx).results().next());
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && name == "structuredClone"
        {
            let [arg] = args else {
                bail!("structuredClone requires one argument");
            };
            let val = self.expression(ctx, arg)?;
            return self.clone_structured_value(ctx, val).map(Some);
        }
        let callee = match callee {
            Expr::LocalGet(id) => self.closures.get(id).copied().unwrap_or(callee),
            _ => callee,
        };
        if matches!(callee, Expr::Closure { .. }) {
            let mut arg_vals = Vec::with_capacity(args.len());
            for arg in args {
                arg_vals.push(self.expression(ctx, arg)?);
            }
            return self.array_callback(ctx, callee, &arg_vals).map(Some);
        }
        let Expr::FuncRef(id) = callee else {
            return Err(match callee {
                Expr::LocalGet(id) => {
                    let error = anyhow!(
                        "indirect calls through local or parameter bindings are not supported yet; call a declared module function directly"
                    );
                    let error = self.shapes.annotate_call(error, byte_offset);
                    self.shapes.annotate_local(error, *id)
                }
                Expr::Closure { .. } => anyhow!(
                    "immediately invoked function expressions and closure calls are not supported yet; declare a named function instead"
                ),
                Expr::ExternFuncRef { name, .. } => {
                    anyhow!("call to external or unresolved function '{name}' is not supported yet")
                }
                Expr::IndexGet { .. } => {
                    anyhow!("computed method or element calls are not supported yet")
                }
                _ => anyhow!(
                    "indirect function calls are not supported yet; call a declared module function directly"
                ),
            });
        };
        let signature = self
            .signatures
            .get(id)
            .context("call to an unknown function")?;
        let (fixed, rest) = signature.parameters();
        ensure!(
            rest.is_some() || args.len() <= fixed.len(),
            "call to {} with {} arguments, expected {}",
            signature.name,
            args.len(),
            fixed.len()
        );
        let mut values = Vec::with_capacity(signature.params.len());
        for (i, param) in fixed.iter().enumerate() {
            values.push(
                self.bind_fixed_arg(ctx, param, args.get(i))
                    .with_context(|| format!("argument {} of {}", i + 1, signature.name))?,
            );
        }
        if let Some(rest) = rest {
            let array = TypedHandle::<ArrayType>::from_handle(rest.binding.carrier, ctx)
                .context("rest parameter must have an array type")?;
            let element = array.deref(ctx).element();
            let mut items = Vec::with_capacity(args.len().saturating_sub(fixed.len()));
            for arg in args.iter().skip(fixed.len()) {
                let value = self.expression_with(ctx, arg, Some(element))?;
                items.push(self.fit(ctx, value, element)?);
            }
            values.push(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, items)));
        }
        self.emit_signature_call(ctx, signature, values)
    }

    pub(super) fn bind_fixed_arg(
        &mut self,
        ctx: &mut Context,
        param: &program::ParamSig<'s>,
        arg: Option<&'s Expr>,
    ) -> Result<Value> {
        let carrier = param.binding.carrier;
        if let Some(arg) = arg {
            if matches!(param.source.ty, HirType::Never) {
                let value = self.expression(ctx, arg)?;
                return Ok(if value.get_type(ctx) == carrier {
                    value
                } else {
                    self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, carrier))
                });
            }
            let value = self.expression_with(ctx, arg, Some(carrier))?;
            return self.fit(ctx, value, carrier);
        }
        if admits_undefined(ctx, carrier) {
            return Ok(self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, carrier)));
        }
        if let Some(default) = &param.source.default {
            let value = self.expression_with(ctx, default, Some(carrier))?;
            return self.fit(ctx, value, carrier);
        }
        bail!("missing required argument")
    }

    pub(super) fn emit_signature_call(
        &mut self,
        ctx: &mut Context,
        signature: &Signature<'s>,
        values: Vec<Value>,
    ) -> Result<Option<Value>> {
        if signature.is_async {
            let result = signature
                .result
                .context("async function must settle one result")?;
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

    pub(super) fn clone_structured_value(
        &mut self,
        ctx: &mut Context,
        val: Value,
    ) -> Result<Value> {
        let ty = val.get_type(ctx);
        if TypedHandle::<ArrayType>::from_handle(ty, ctx).is_ok() {
            return Ok(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![val])));
        }
        if let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            let fields = rec
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .to_vec();
            let mut field_vals = Vec::with_capacity(fields.len());
            for (field_name, field_ty) in fields {
                let can_omit = admits_undefined(ctx, field_ty);
                let op = FieldGetOp::new(ctx, val, field_name.clone())?;
                let raw = self.field_read(ctx, &op)?;
                let cloned = self.clone_structured_value(ctx, raw)?;
                let cloned_ty = cloned.get_type(ctx);
                let field_val = if can_omit && admits_undefined(ctx, cloned_ty) {
                    let op = FieldPresentOp::new(ctx, val, field_name)?;
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

    /// The array literal given to a promise combinator: plain values among promises of `T`
    /// become promises fulfilled with them, as `Promise.all` treats them.
    fn combinator_inputs(&mut self, ctx: &mut Context, items: &'s [Expr]) -> Result<Value> {
        // `Promise.reject(e)` takes its settled type from the other inputs: lower it last.
        let mut inputs = Vec::with_capacity(items.len());
        for item in items {
            inputs.push(match promise_call(item) {
                Some(("reject", reason)) => CombinatorInput::Reject(reason),
                _ => {
                    let value = self.expression(ctx, item)?;
                    CombinatorInput::Lowered(self.defined(ctx, value)?)
                }
            });
        }
        let mut known = inputs.iter().filter_map(|input| match input {
            CombinatorInput::Lowered(value) => Some(*value),
            CombinatorInput::Reject(_) => None,
        });
        let promised = known.clone().find_map(|value| {
            TypedHandle::<PromiseType>::from_handle(value.get_type(ctx), ctx)
                .ok()
                .map(|promise| promise.deref(ctx).value())
        });
        let settled = match (promised, known.next()) {
            (Some(settled), _) => settled,
            (None, Some(first)) => first.get_type(ctx),
            (None, None) => void_type(ctx),
        };
        let promise: TypeHandle = PromiseType::get(ctx, settled).into();
        let mut values = Vec::with_capacity(items.len());
        for input in inputs {
            values.push(match input {
                CombinatorInput::Lowered(value) => value,
                CombinatorInput::Reject(reason) => {
                    self.promise_settled(ctx, "reject", reason, Some(settled))?
                }
            });
        }
        let mut inputs = Vec::with_capacity(values.len());
        for value in values {
            let value = if value.get_type(ctx) == promise {
                value
            } else {
                let value = self.fit(ctx, value, settled)?;
                self.emit_value(ctx, |ctx| PromiseResolvedOp::new(ctx, value))
            };
            inputs.push(value);
        }
        Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, promise, inputs)))
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
            if TypedHandle::<PromiseType>::from_handle(value.get_type(ctx), ctx).is_ok() {
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
