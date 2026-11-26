//! Array and string spread and `concat` lowering.
use super::*;
use crate::lower::control_flow::{ArrayOutput, Holes, Input};
use perry_mir::{ArrayExtendOp, ConcatSegmentsOp, StringType};

impl<'s> FunctionBuilder<'s, '_> {
    /// `[...text]` and `Array.from(text)`: one string per Unicode scalar value.
    pub(crate) fn string_characters(&mut self, ctx: &mut Context, text: Value) -> Value {
        let empty = self.string(ctx, "");
        self.intrinsic(ctx, Intrinsic::StringSplit, vec![text, empty])
    }

    pub(super) fn spread_value(
        &mut self,
        ctx: &mut Context,
        source: &'s Expr,
        element: Option<TypeHandle>,
    ) -> Result<Value> {
        let val = self.spread_operand(ctx, source, element)?;
        self.spread_items(ctx, val, element)
    }

    pub(super) fn spread_operand(
        &mut self,
        ctx: &mut Context,
        source: &'s Expr,
        element: Option<TypeHandle>,
    ) -> Result<Value> {
        let expected = if matches!(source, Expr::Array(items, _) if items.is_empty()) {
            element.map(|element| ArrayType::get(ctx, element).into())
        } else {
            None
        };
        self.expression_with(ctx, source, expected)
    }

    pub(super) fn spread_items(
        &mut self,
        ctx: &mut Context,
        val: Value,
        element: Option<TypeHandle>,
    ) -> Result<Value> {
        let val_ty = optional_value(ctx, val.get_type(ctx)).unwrap_or_else(|| val.get_type(ctx));
        if val_ty.deref(ctx).is::<StringType>() {
            let val = self.defined(ctx, val)?;
            return Ok(self.string_characters(ctx, val));
        }
        if crate::lower::collections::is_collection_or_view_record(ctx, val_ty)
            || crate::lower::collections::set_entry_type(ctx, val_ty).is_some()
            || crate::lower::collections::map_entry_types(ctx, val_ty).is_some()
        {
            let val = self.defined(ctx, val)?;
            return self.collect_items(ctx, Input::Collection(val), |_, _, item, _, _| {
                Ok(ArrayOutput::Item(item))
            });
        }
        let val = self.admit_holes(ctx, val, element)?;
        self.without_holes(ctx, val, Holes::Dense, false)
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
        let ty = value.get_type(ctx);
        let target = ArrayType::get(ctx, element).into();
        if ty.deref(ctx).is::<ArrayType>() {
            self.fit(ctx, value, target)
        } else if present_type(ctx, ty).deref(ctx).is::<ArrayType>() {
            // An absent array argument, such as a spread hole, is appended as one element.
            let states = absence(ctx, element);
            let absent = |null: bool| {
                move |b: &mut Self, ctx: &mut Context| {
                    let item = match states {
                        Some(s) if null && s.admits_null() => {
                            b.emit_value(ctx, |ctx| NullOp::new(ctx, element))
                        }
                        Some(s) if !null && s.admits_undefined() => {
                            b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, element))
                        }
                        // The element type excludes it, so `defined` throws `TypeError`.
                        _ => {
                            let array = b.defined(ctx, value)?;
                            return b.fit(ctx, array, target);
                        }
                    };
                    Ok(b.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![item])))
                }
            };
            self.match_optional(
                ctx,
                value,
                target,
                absent(false),
                absent(true),
                |b, ctx, array| b.fit(ctx, array, target),
            )
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
        let element = expected
            .and_then(|ty| perry_mir::typed::<ArrayType>(ctx, present_type(ctx, ty)))
            .map(|array| array.deref(ctx).element())
            .ok_or_else(|| {
                Diagnostic::new(
                    codes::MISSING_TYPE_ANNOTATION,
                    "array spread needs an element type",
                )
            })?;
        let mut result = None;
        // Holes evaluate nothing, so each run is appended once the element type is known.
        let mut holes = 0;
        for item in items {
            let (expression, spread) = match item {
                perry_hir::ArrayElement::Expr(expression) => (expression, false),
                perry_hir::ArrayElement::Spread(expression) => (expression, true),
                perry_hir::ArrayElement::Hole => {
                    holes += 1;
                    continue;
                }
            };
            let value = if spread {
                self.spread_value(ctx, expression, Some(element))?
            } else {
                self.expression_with(ctx, expression, Some(element))?
            };
            let source = if spread {
                let target = ArrayType::get(ctx, element).into();
                self.fit(ctx, value, target)?
            } else {
                let value = self.fit(ctx, value, element)?;
                self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![value]))
            };
            result = self.append_holes(ctx, result, element, std::mem::take(&mut holes));
            let sources = result.map_or_else(|| vec![source], |prefix| vec![prefix, source]);
            result = Some(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, sources)));
        }
        if holes > 0 {
            result = self.append_holes(ctx, result, element, holes);
        }
        result.ok_or_else(|| {
            Diagnostic::new(
                codes::MISSING_TYPE_ANNOTATION,
                "an empty spread array needs an element type",
            )
        })
    }

    /// `prefix` followed by `count` holes.
    fn append_holes(
        &mut self,
        ctx: &mut Context,
        prefix: Option<Value>,
        element: TypeHandle,
        count: u32,
    ) -> Option<Value> {
        if count == 0 {
            return prefix;
        }
        let holes = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
        for index in 0..count {
            let index = self.number(ctx, f64::from(index));
            self.emit(ctx, |ctx| perry_mir::ArrayHoleOp::new(ctx, holes, index));
        }
        Some(prefix.map_or(holes, |prefix| {
            self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, vec![prefix, holes]))
        }))
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
            let signature = self.signature(*id)?;
            let result = self.invoke_arguments(
                ctx,
                signature,
                std::iter::empty(),
                Arguments::Spread(args),
                None,
            )?;
            return result.map_or_else(|| self.absent_literal(ctx, false, None), Ok);
        }
        let Expr::PropertyGet {
            object, property, ..
        } = callee
        else {
            bail_unsupported!("spread calls require a dense array push receiver")
        };
        let method_name = hir_compat::method(property).name();
        if matches!(method_name, "max" | "min") {
            return self.math_extremum_spread(ctx, method_name == "max", Arguments::Spread(args));
        }
        ensure_unsupported!(
            matches!(method_name, "push" | "concat"),
            "spread calls are supported for dense array push"
        );
        if method_name == "concat" {
            return self.concat_call_spread(ctx, object, args);
        }
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        ensure_unsupported!(
            kind(ctx, receiver)? == Kind::Array,
            "push requires a dense array receiver"
        );
        let element = self.array_element(ctx, receiver)?;
        let values = self.collect_spread_args(ctx, args, element)?;
        self.emit(ctx, |ctx| ArrayExtendOp::new(ctx, receiver, values));
        Ok(self.array_length(ctx, receiver))
    }

    pub(super) fn math_extremum_spread(
        &mut self,
        ctx: &mut Context,
        maximum: bool,
        args: Arguments<'s>,
    ) -> Result<Value> {
        let (init, op) = if maximum {
            (f64::NEG_INFINITY, Intrinsic::MathMax)
        } else {
            (f64::INFINITY, Intrinsic::MathMin)
        };
        let number = FP64Type::get(ctx).into();
        let result = self.entry_slot(ctx, number);
        let init = self.number(ctx, init);
        self.store(ctx, result, init);
        self.for_each_spread_arg(ctx, args, number, "math_spread", |b, ctx, item| {
            let value = b.arithmetic(ctx, item);
            let previous = b.load(ctx, result)?;
            let next = b.intrinsic(ctx, op, vec![previous, value]);
            b.store(ctx, result, next);
            Ok(())
        })?;
        self.load(ctx, result)
    }

    fn for_each_spread_arg(
        &mut self,
        ctx: &mut Context,
        args: Arguments<'s>,
        hint: TypeHandle,
        label: &str,
        mut on_value: impl FnMut(&mut Self, &mut Context, Value) -> Result<()>,
    ) -> Result<()> {
        for (expression, spread) in args {
            if !spread {
                let value = self.expression(ctx, expression)?;
                on_value(self, ctx, value)?;
            } else {
                let source = self.spread_value(ctx, expression, Some(hint))?;
                let source = self.defined(ctx, source)?;
                self.array_element(ctx, source)?;
                let limit = self.array_length(ctx, source);
                let zero = self.number(ctx, 0.0);
                self.index_loop(ctx, label, zero, limit, |b, ctx, current, _, _| {
                    let value = b.element_at(ctx, source, current)?;
                    on_value(b, ctx, value)
                })?;
            }
        }
        Ok(())
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
            let text = StringType::get(ctx).into();
            let result = self.entry_slot(ctx, text);
            self.store(ctx, result, receiver);
            self.for_each_spread_arg(
                ctx,
                Arguments::Spread(args),
                text,
                "string_concat_arguments",
                |b, ctx, value| {
                    let value = b.display(ctx, value)?;
                    let previous = b.load(ctx, result)?;
                    let joined = b.emit_value(ctx, |ctx| ConcatOp::new(ctx, previous, value));
                    b.store(ctx, result, joined);
                    Ok(())
                },
            )?;
            return self.load(ctx, result);
        }
        let element = self.array_element(ctx, receiver)?;
        let array = ArrayType::get(ctx, element).into();
        let sources = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, array, vec![receiver]));
        self.for_each_spread_arg(
            ctx,
            Arguments::Spread(args),
            element,
            "concat_arguments",
            |b, ctx, value| {
                let segment = b.concat_segment(ctx, value, element)?;
                b.emit(ctx, |ctx| ArrayPushOp::new(ctx, sources, segment));
                Ok(())
            },
        )?;
        Ok(self.emit_value(ctx, |ctx| ConcatSegmentsOp::new(ctx, sources)))
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
}
