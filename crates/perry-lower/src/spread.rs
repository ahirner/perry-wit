//! Array and string spread and `concat` lowering.
use super::*;
use crate::array_methods::{CONCAT, PUSH};
use perry_mir::{
    attrs::Absence,
    collections::{ArrayExtendOp, ConcatSegmentsOp},
    types::{OptionalType, StringType},
};

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
            return self.materialize_collection_to_array(ctx, val);
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
            .and_then(|ty| {
                TypedHandle::<ArrayType>::from_handle(optional_value(ctx, ty).unwrap_or(ty), ctx)
                    .ok()
            })
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
                    match ty {
                        HirType::Array(inner) => *inner,
                        HirType::Generic {
                            base,
                            mut type_args,
                        } if matches!(base.as_str(), "Set" | "ReadonlySet")
                            && type_args.len() == 1 =>
                        {
                            type_args.pop().unwrap()
                        }
                        HirType::Generic {
                            base,
                            mut type_args,
                        } if base == "MapKeys" && type_args.len() == 2 => {
                            type_args.swap_remove(0)
                        }
                        HirType::Generic {
                            base,
                            mut type_args,
                        } if base == "MapValues" && type_args.len() == 2 => {
                            type_args.pop().unwrap()
                        }
                        HirType::Generic {
                            base,
                            mut type_args,
                        } if base == "SetEntries" && type_args.len() == 1 => {
                            let t = type_args.pop().unwrap();
                            HirType::Tuple(vec![t.clone(), t])
                        }
                        HirType::Generic { base, type_args }
                            if matches!(base.as_str(), "Map" | "ReadonlyMap")
                                && type_args.len() == 2 =>
                        {
                            HirType::Tuple(type_args)
                        }
                        _ => return None,
                    }
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
                let inner = optional_value(ctx, base).unwrap_or(base);
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
                let inner = optional_value(ctx, previous).unwrap_or(previous);
                ensure!(
                    optional_value(ctx, source_element).unwrap_or(source_element) == inner,
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
        let Expr::PropertyGet {
            object, property, ..
        } = callee
        else {
            bail!("spread calls require a dense array push receiver")
        };
        ensure!(
            matches!(property.as_str(), PUSH | "push" | CONCAT | "concat"),
            "spread calls are supported for dense array push"
        );
        if matches!(property.as_str(), CONCAT | "concat") {
            return self.concat_call_spread(ctx, object, args);
        }
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        ensure!(
            kind(ctx, receiver)? == Kind::Array,
            "push requires a dense array receiver"
        );
        let element = self.array_element(ctx, receiver)?;
        let values = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
        for argument in args {
            match argument {
                perry_hir::CallArg::Expr(expression) => {
                    let value = self.expression_with(ctx, expression, Some(element))?;
                    let value = self.fit(ctx, value, element)?;
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, values, value));
                }
                perry_hir::CallArg::Spread(expression) => {
                    let source = self.spread_value(ctx, expression, Some(element))?;
                    let source = self.defined(ctx, source)?;
                    let target = ArrayType::get(ctx, element).into();
                    let source = self.fit(ctx, source, target)?;
                    self.append_array(ctx, values, source)?;
                }
            }
        }
        self.append_array(ctx, receiver, values)?;
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
        let sources = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, text, vec![]));
        for argument in args {
            match argument {
                perry_hir::CallArg::Expr(expression) => {
                    let value = self.expression(ctx, expression)?;
                    let value = self.display(ctx, value)?;
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, sources, value));
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
                            b.emit(ctx, |ctx| ArrayPushOp::new(ctx, sources, value));
                            Ok(())
                        },
                    )?;
                }
            }
        }
        let result = self.entry_slot(ctx, text);
        self.store(ctx, result, receiver);
        let limit = self.array_length(ctx, sources);
        let zero = self.number(ctx, 0.0);
        self.index_loop(
            ctx,
            "string_concat_segments",
            zero,
            limit,
            |b, ctx, current, _, _| {
                let previous = b.load(ctx, result)?;
                let value = b.element_at(ctx, sources, current)?;
                let joined = b.emit_value(ctx, |ctx| ConcatOp::new(ctx, previous, value));
                b.store(ctx, result, joined);
                Ok(())
            },
        )?;
        self.load(ctx, result)
    }

    /// Push every element of `source` onto `target`.
    fn append_array(&mut self, ctx: &mut Context, target: Value, source: Value) -> Result<()> {
        self.emit(ctx, |ctx| ArrayExtendOp::new(ctx, target, source));
        Ok(())
    }
}
