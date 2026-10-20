//! Array and string spread and `concat` lowering.
use super::*;
use crate::control_flow::{ArrayOutput, Input};
use perry_mir::{Absence, ArrayExtendOp, ConcatSegmentsOp, OptionalType, StringType};

impl<'s> FunctionBuilder<'s> {
    fn spread_value(
        &mut self,
        ctx: &mut Context,
        source: &'s Expr,
        element: Option<TypeHandle>,
    ) -> Result<Value> {
        let expected = if matches!(source, Expr::Array(items) if items.is_empty()) {
            element.map(|element| ArrayType::get(ctx, element).into())
        } else {
            None
        };
        let val = self.expression_with(ctx, source, expected)?;
        let val_ty = optional_value(ctx, val.get_type(ctx)).unwrap_or_else(|| val.get_type(ctx));
        if crate::collections::is_collection_or_view_record(ctx, val_ty)
            || crate::collections::set_entry_type(ctx, val_ty).is_some()
            || crate::collections::map_entry_types(ctx, val_ty).is_some()
        {
            let val = self.defined(ctx, val)?;
            return self.collect_items(ctx, Input::Collection(val), |_, _, item, _, _| {
                Ok(ArrayOutput::Item(item))
            });
        }
        Ok(val)
    }

    pub(super) fn concat_values(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        args: &'s [Expr],
    ) -> Result<Value> {
        let element = self.array_element(ctx, receiver)?;
        let mut operands = Vec::with_capacity(args.len() + 1);
        operands.push(receiver);
        for arg in args {
            let value = self.expression(ctx, arg)?;
            operands.push(self.concat_segment(ctx, value, element)?);
        }
        Ok(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, operands)))
    }

    fn concat_segment(
        &mut self,
        ctx: &mut Context,
        value: Value,
        element: TypeHandle,
    ) -> Result<Value> {
        if value.get_type(ctx).deref(ctx).is::<ArrayType>() {
            let target = ArrayType::get(ctx, element).into();
            self.fit(ctx, value, target)
        } else {
            let value = self.fit(ctx, value, element)?;
            Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![value])))
        }
    }

    pub(super) fn array_spread(
        &mut self,
        ctx: &mut Context,
        items: &'s [perry_hir::ArrayElement],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let mut element = expected
            .and_then(|ty| TypedHandle::<ArrayType>::from_handle(present_type(ctx, ty), ctx).ok())
            .map(|array| array.deref(ctx).element());
        let fixed_element = element.is_some();
        if element.is_none() {
            let env = &self.shapes.type_env;
            element = items.iter().find_map(|item| {
                let (expression, spread) = match item {
                    perry_hir::ArrayElement::Expr(Expr::Null | Expr::Undefined)
                    | perry_hir::ArrayElement::Hole => return None,
                    perry_hir::ArrayElement::Expr(expr) => (expr, false),
                    perry_hir::ArrayElement::Spread(expr) => (expr, true),
                };
                let ty = perry_hir::infer_expr_type(expression, env);
                let ty = if spread {
                    crate::shapes::generics::iterated_element(ty).ok()?
                } else {
                    ty
                };
                self.shapes.value_type(ctx, &ty).ok()
            });
            if let Some(base) = element {
                let states = items.iter().fold(absence(ctx, base), |states, item| {
                    let state = match item {
                        perry_hir::ArrayElement::Expr(Expr::Null) => Some(Absence::Null),
                        perry_hir::ArrayElement::Expr(Expr::Undefined) => Some(Absence::Undefined),
                        _ => None,
                    };
                    crate::coerce::merge_absence(states, state)
                });
                let inner = present_type(ctx, base);
                element = Some(states.map_or(inner, |states| {
                    OptionalType::get_with(ctx, inner, states).into()
                }));
            }
        }
        let mut result = None;
        for item in items {
            let (expression, spread) = match item {
                perry_hir::ArrayElement::Expr(expression) => (expression, false),
                perry_hir::ArrayElement::Spread(expression) => (expression, true),
                perry_hir::ArrayElement::Hole => {
                    return Err(PerryDiagnostic::error(
                        DiagnosticCode::UnsupportedSpread,
                        "sparse array spreads are outside the dense-array profile",
                    )
                    .into());
                }
            };
            let value = if spread {
                self.spread_value(ctx, expression, element)?
            } else {
                self.expression_with(ctx, expression, element)?
            };
            let source_element = if spread {
                self.array_element(ctx, value)?
            } else {
                value.get_type(ctx)
            };
            if !fixed_element && let Some(previous) = element {
                let inner = present_type(ctx, previous);
                ensure!(
                    present_type(ctx, source_element) == inner,
                    "array spreads must have one element type"
                );
                let merged = crate::coerce::merge_absence(
                    absence(ctx, previous),
                    absence(ctx, source_element),
                )
                .map_or(inner, |states| {
                    OptionalType::get_with(ctx, inner, states).into()
                });
                element = Some(merged);
                if let Some(prefix) = result {
                    let target = ArrayType::get(ctx, merged).into();
                    result = Some(self.fit(ctx, prefix, target)?);
                }
            }
            let element = *element.get_or_insert(source_element);
            let source = if spread {
                let target = ArrayType::get(ctx, element).into();
                self.fit(ctx, value, target)?
            } else {
                let value = self.fit(ctx, value, element)?;
                self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![value]))
            };
            let sources = result.map_or_else(|| vec![source], |prefix| vec![prefix, source]);
            result = Some(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, sources)));
        }
        result.context("an empty spread array needs an element type")
    }

    /// `array.push(…)` with spread arguments: all values are collected before the receiver
    /// grows, so spreading the receiver into itself appends its original elements.
    pub(super) fn array_push_spread(
        &mut self,
        ctx: &mut Context,
        callee: &'s Expr,
        args: &'s [perry_hir::CallArg],
    ) -> Result<Value> {
        if let Expr::FuncRef(id) = callee {
            return self.func_call_spread(ctx, *id, args);
        }
        let Expr::PropertyGet {
            object, property, ..
        } = callee
        else {
            bail!("spread calls require a dense array push receiver")
        };
        let method_name = hir_compat::method(property).name();
        if matches!(method_name, "max" | "min") {
            let (init_val, op) = if method_name == "max" {
                (f64::NEG_INFINITY, Intrinsic::MathMax)
            } else {
                (f64::INFINITY, Intrinsic::MathMin)
            };
            let init = self.number(ctx, init_val);
            let acc_slot = self.entry_slot(ctx, FP64Type::get(ctx).into());
            self.store(ctx, acc_slot, init);
            for argument in args {
                match argument {
                    perry_hir::CallArg::Expr(expr) => {
                        let val = self.number_operand(ctx, expr)?;
                        let cur = self.load(ctx, acc_slot)?;
                        let next = self.intrinsic(ctx, op, vec![cur, val]);
                        self.store(ctx, acc_slot, next);
                    }
                    perry_hir::CallArg::Spread(expr) => {
                        self.fold_math_spread(ctx, op, acc_slot, expr)?;
                    }
                }
            }
            return self.load(ctx, acc_slot);
        }
        ensure!(
            matches!(method_name, "push" | "concat"),
            "spread calls are supported for dense array push"
        );
        if method_name == "concat" {
            return self.concat_call_spread(ctx, object, args);
        }
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        ensure!(
            kind(ctx, receiver)? == Kind::Array,
            "push requires a dense array receiver"
        );
        let element = self.array_element(ctx, receiver)?;
        let values = self.collect_spread_args(ctx, args, element)?;
        self.emit(ctx, |ctx| ArrayExtendOp::new(ctx, receiver, values));
        Ok(self.array_length(ctx, receiver))
    }

    fn concat_call_spread(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        args: &'s [perry_hir::CallArg],
    ) -> Result<Value> {
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        if receiver.get_type(ctx).deref(ctx).is::<StringType>() {
            return self.string_concat_spread(ctx, receiver, args);
        }
        let element = self.array_element(ctx, receiver)?;
        let array = ArrayType::get(ctx, element).into();
        let sources = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, array, vec![receiver]));
        for argument in args {
            match argument {
                perry_hir::CallArg::Expr(expression) => {
                    let value = self.expression(ctx, expression)?;
                    let source = self.concat_segment(ctx, value, element)?;
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, sources, source));
                }
                perry_hir::CallArg::Spread(expression) => {
                    let source = self.spread_value(ctx, expression, Some(element))?;
                    self.array_element(ctx, source)?;
                    let limit = self.array_length(ctx, source);
                    let zero = self.number(ctx, 0.0);
                    self.index_loop(
                        ctx,
                        "concat_arguments",
                        zero,
                        limit,
                        |b, ctx, current, _, _| {
                            let value = b.element_at(ctx, source, current)?;
                            let segment = b.concat_segment(ctx, value, element)?;
                            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, sources, segment));
                            Ok(())
                        },
                    )?;
                }
            }
        }
        Ok(self.emit_value(ctx, |ctx| ConcatSegmentsOp::new(ctx, sources)))
    }

    fn string_concat_spread(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        args: &'s [perry_hir::CallArg],
    ) -> Result<Value> {
        let text = StringType::get(ctx).into();
        let result = self.entry_slot(ctx, text);
        self.store(ctx, result, receiver);
        for argument in args {
            match argument {
                perry_hir::CallArg::Expr(expression) => {
                    let value = self.expression(ctx, expression)?;
                    let value = self.display(ctx, value)?;
                    let previous = self.load(ctx, result)?;
                    let joined = self.emit_value(ctx, |ctx| ConcatOp::new(ctx, previous, value));
                    self.store(ctx, result, joined);
                }
                perry_hir::CallArg::Spread(expression) => {
                    let source = self.spread_value(ctx, expression, Some(text))?;
                    let source = self.defined(ctx, source)?;
                    self.array_element(ctx, source)?;
                    let limit = self.array_length(ctx, source);
                    let zero = self.number(ctx, 0.0);
                    self.index_loop(
                        ctx,
                        "string_concat_arguments",
                        zero,
                        limit,
                        |b, ctx, current, _, _| {
                            let value = b.element_at(ctx, source, current)?;
                            let value = b.display(ctx, value)?;
                            let previous = b.load(ctx, result)?;
                            let joined =
                                b.emit_value(ctx, |ctx| ConcatOp::new(ctx, previous, value));
                            b.store(ctx, result, joined);
                            Ok(())
                        },
                    )?;
                }
            }
        }
        self.load(ctx, result)
    }

    fn collect_spread_args(
        &mut self,
        ctx: &mut Context,
        args: &'s [perry_hir::CallArg],
        elem_ty: TypeHandle,
    ) -> Result<Value> {
        let arr = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, vec![]));
        for argument in args {
            match argument {
                perry_hir::CallArg::Expr(expr) => {
                    let v = self.expression_with(ctx, expr, Some(elem_ty))?;
                    let v = self.fit(ctx, v, elem_ty)?;
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, arr, v));
                }
                perry_hir::CallArg::Spread(expr) => {
                    let src = self.spread_value(ctx, expr, Some(elem_ty))?;
                    let src = self.defined(ctx, src)?;
                    let target_arr_ty = ArrayType::get(ctx, elem_ty).into();
                    let src = self.fit(ctx, src, target_arr_ty)?;
                    self.emit(ctx, |ctx| ArrayExtendOp::new(ctx, arr, src));
                }
            }
        }
        Ok(arr)
    }

    fn func_call_spread(
        &mut self,
        ctx: &mut Context,
        id: FuncId,
        args: &'s [perry_hir::CallArg],
    ) -> Result<Value> {
        let signature = self
            .signatures
            .get(&id)
            .context("call to an unknown function")?;
        let (params, rest) = signature.parameters();
        let fixed_len = params.len();
        let mut values = Vec::with_capacity(signature.params.len());
        let mut arg_idx = 0usize;
        while values.len() < fixed_len && arg_idx < args.len() {
            let param = &params[values.len()];
            match &args[arg_idx] {
                perry_hir::CallArg::Expr(expr) => {
                    values.push(self.bind_fixed_arg(ctx, param, Some(expr))?);
                    arg_idx += 1;
                }
                perry_hir::CallArg::Spread(expr) => {
                    let src = self.expression(ctx, expr)?;
                    let src = self.defined(ctx, src)?;
                    if let Ok(rec) = TypedHandle::<RecordType>::from_handle(src.get_type(ctx), ctx)
                        && rec.deref(ctx).kind() == RecordKind::Tuple
                    {
                        let fields_len = rec.deref(ctx).fields()?.len();
                        for tuple_idx in 0..fields_len {
                            if values.len() >= fixed_len {
                                break;
                            }
                            let p = &params[values.len()];
                            let op = FieldGetOp::new(ctx, src, format!("_{tuple_idx}"))?;
                            let loaded = self.field_read(ctx, &op)?;
                            values.push(self.fit(ctx, loaded, p.binding.carrier)?);
                        }
                        arg_idx += 1;
                    } else {
                        bail!("spreading into fixed parameters requires a tuple");
                    }
                }
            }
        }
        while values.len() < fixed_len {
            let param = &params[values.len()];
            values.push(self.bind_fixed_arg(ctx, param, None).with_context(|| {
                format!(
                    "missing required argument in spread call to {}",
                    signature.name
                )
            })?);
        }
        if let Some(rest_param) = rest {
            let rest_arr_ty =
                TypedHandle::<ArrayType>::from_handle(rest_param.binding.carrier, ctx)
                    .context("rest parameter must have an array type")?;
            let elem_ty = rest_arr_ty.deref(ctx).element();
            values.push(self.collect_spread_args(ctx, &args[arg_idx..], elem_ty)?);
        }
        Ok(self
            .emit_signature_call(ctx, signature, values)?
            .unwrap_or_else(|| {
                let void = void_type(ctx);
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, void))
            }))
    }
}
