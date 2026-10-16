//! Calls: module functions, WIT imports, imported built-ins and `Promise` statics.
use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn call(
        &mut self,
        ctx: &mut Context,
        callee: &'s Expr,
        args: &'s [Expr],
        byte_offset: u32,
    ) -> Result<Option<Value>> {
        if let Some(call) = hir_compat::runtime_call(callee, args) {
            use hir_compat::RuntimeCall;
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
                    let wrapped =
                        perry_mir::types::OptionalType::get_with(ctx, actual, states).into();
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
            let op = perry_mir::foreign::ForeignOp::new(
                ctx,
                perry_mir::foreign::Phase::Call,
                &binding,
                values,
                results,
            );
            self.append(ctx, &op);
            return Ok(op.get_operation().deref(ctx).results().next());
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
        let name = &signature.name;
        let params = &signature.params;
        ensure!(
            args.len() == params.len(),
            "call to {name} with {} arguments, expected {}",
            args.len(),
            params.len()
        );
        let values = args
            .iter()
            .zip(params)
            .map(|(arg, param)| {
                let value = self.expression_with(ctx, arg, Some(param.carrier))?;
                self.fit(ctx, value, param.carrier)
            })
            .collect::<Result<Vec<_>>>()?;
        if signature.is_async {
            let Some(result) = signature.result else {
                bail!("async function {name} settles one result");
            };
            let promise = self.emit_value(ctx, |ctx| {
                AsyncCallOp::new(ctx, name.clone(), values, result.carrier)
            });
            return Ok(Some(promise));
        }
        let op = CallOp::new(ctx, name.clone(), values, signature.carrier_results());
        self.append(ctx, &op);
        let result = {
            let operation = op.get_operation().deref(ctx);
            (operation.get_num_results() == 1).then(|| operation.get_result(0))
        };
        if let (Some(mut value), Some(result_ty)) = (result, signature.result) {
            if let Some(int_ty) = result_ty.integer {
                value = self.wrap_integer(ctx, value, int_ty, result_ty.carrier);
            }
            return Ok(Some(value));
        }
        Ok(result)
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
