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
        if crate::lower::collections::Collection::of(ctx, val_ty).is_some() {
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
        for item in items {
            let arg = match item {
                perry_hir::ArrayElement::Expr(expr) => (expr, false),
                perry_hir::ArrayElement::Spread(expr) => (expr, true),
                perry_hir::ArrayElement::Hole => {
                    let array = *result.get_or_insert_with(|| {
                        self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]))
                    });
                    let end = self.array_length(ctx, array);
                    self.emit(ctx, |ctx| perry_mir::ArrayHoleOp::new(ctx, array, end));
                    continue;
                }
            };
            let segment = match self.array_argument(ctx, element, arg)? {
                ArrayOutput::Items(items) => items,
                ArrayOutput::Item(value) => {
                    self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![value]))
                }
            };
            let sources = result.into_iter().chain([segment]).collect();
            result = Some(self.emit_value(ctx, |ctx| ConcatOp::from_sources(ctx, sources)));
        }
        result.ok_or_else(|| {
            diag!(
                codes::MISSING_TYPE_ANNOTATION,
                "an empty spread array needs an element type"
            )
        })
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
            return self.math_extremum(ctx, method_name == "max", Arguments::Spread(args));
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
        let values = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
        for arg in Arguments::Spread(args) {
            self.array_argument(ctx, element, arg)?
                .append_to(self, ctx, values);
        }
        self.emit(ctx, |ctx| ArrayExtendOp::new(ctx, receiver, values));
        Ok(self.array_length(ctx, receiver))
    }

    pub(super) fn math_extremum(
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
        let init = self.number(ctx, init);
        self.fold_arguments(ctx, args, init, |b, ctx, previous, item| {
            let value = b.require_number(ctx, item)?;
            Ok(b.intrinsic(ctx, op, vec![previous, value]))
        })
    }

    pub(super) fn fold_arguments(
        &mut self,
        ctx: &mut Context,
        args: Arguments<'s>,
        initial: Value,
        mut step: impl FnMut(&mut Self, &mut Context, Value, Value) -> Result<Value>,
    ) -> Result<Value> {
        if let Arguments::Plain(args) = args {
            return args.iter().try_fold(initial, |previous, arg| {
                let value = self.expression(ctx, arg)?;
                step(self, ctx, previous, value)
            });
        }
        let ty = initial.get_type(ctx);
        let slot = self.entry_slot(ctx, ty);
        self.store(ctx, slot, initial);
        self.for_each_spread_arg(ctx, args, ty, |b, ctx, item| {
            let previous = b.load(ctx, slot)?;
            let value = step(b, ctx, previous, item)?;
            b.store(ctx, slot, value);
            Ok(())
        })?;
        self.load(ctx, slot)
    }

    fn for_each_spread_arg(
        &mut self,
        ctx: &mut Context,
        args: Arguments<'s>,
        hint: TypeHandle,
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
                self.index_loop(ctx, "spread", zero, limit, |b, ctx, current, _, _| {
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
            return self.concat_strings(ctx, receiver, Arguments::Spread(args));
        }
        let element = self.array_element(ctx, receiver)?;
        let array = ArrayType::get(ctx, element).into();
        let sources = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, array, vec![receiver]));
        self.for_each_spread_arg(ctx, Arguments::Spread(args), element, |b, ctx, value| {
            let segment = b.concat_segment(ctx, value, element)?;
            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, sources, segment));
            Ok(())
        })?;
        Ok(self.emit_value(ctx, |ctx| ConcatSegmentsOp::new(ctx, sources)))
    }

    pub(super) fn array_argument(
        &mut self,
        ctx: &mut Context,
        element: TypeHandle,
        (expr, spread): (&'s Expr, bool),
    ) -> Result<ArrayOutput> {
        if spread {
            let items = self.spread_value(ctx, expr, Some(element))?;
            let items = self.defined(ctx, items)?;
            Ok(ArrayOutput::Items(self.fit(
                ctx,
                items,
                ArrayType::get(ctx, element).into(),
            )?))
        } else {
            let value = self.expression_with(ctx, expr, Some(element))?;
            Ok(ArrayOutput::Item(self.fit(ctx, value, element)?))
        }
    }
}
