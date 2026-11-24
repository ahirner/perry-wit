//! User-defined class construction, field initialization, methods, accessors, and inheritance.
use super::*;
use crate::inline::InlineResult;
use pliron::utils::vec_exns::VecExtns;

enum ClassArguments<'a, 's> {
    Expressions(&'s [Expr]),
    Evaluated(&'a [Value]),
}

enum ClassReceiver<'a, 's> {
    Static,
    Instance(Value),
    Constructor(
        Value,
        &'s perry_frontend::hir::ir::Class,
        &'a [(&'s str, TypeHandle, Option<&'s Expr>)],
    ),
}

#[derive(Clone, Copy)]
pub(super) enum ClassExpansion<'s> {
    Callable(&'s Function),
    Fields(&'s perry_frontend::hir::ir::Class),
}

impl<'s> FunctionBuilder<'s> {
    fn with_class_expansion<T>(
        &mut self,
        ctx: &mut Context,
        expansion: ClassExpansion<'s>,
        owner_class: &str,
        lower: impl FnOnce(&mut Self, &mut Context) -> Result<T>,
    ) -> Result<T> {
        let recursive = self
            .class_expansions
            .iter()
            .any(|active| match (active, &expansion) {
                (ClassExpansion::Callable(active), ClassExpansion::Callable(next)) => {
                    std::ptr::eq(*active, *next)
                }
                (ClassExpansion::Fields(active), ClassExpansion::Fields(next)) => {
                    std::ptr::eq(*active, *next)
                }
                _ => false,
            });
        if recursive {
            match expansion {
                ClassExpansion::Callable(func) => {
                    bail_unsupported!(
                        "recursive inline class call to {owner_class}.{} is not supported yet",
                        func.name
                    )
                }
                ClassExpansion::Fields(_) => {
                    bail_unsupported!(
                        "recursive inline field initialization of class {owner_class} is not supported yet"
                    )
                }
            }
        }
        self.class_expansions.push(expansion);
        let result = lower(self, ctx);
        self.class_expansions.pop();
        result.map_err(|error| {
            let locations = &self.shapes.metadata.locations;
            let member_span = match expansion {
                ClassExpansion::Callable(func) => {
                    locations.function_span(&format!("{owner_class}::{}", func.name))
                }
                ClassExpansion::Fields(_) => None,
            };
            error.with_fallback_span(member_span.or_else(|| locations.type_span(owner_class)))
        })
    }

    /// `new ClassName<type_args>(args)` for a user-defined class.
    pub(super) fn class_new(
        &mut self,
        ctx: &mut Context,
        class_name: &'s str,
        args: &'s [Expr],
    ) -> Result<Value> {
        let (&class_name, &(_, class)) = self
            .shapes
            .decls
            .classes
            .named
            .get_key_value(class_name)
            .ok_or_else(|| unsupported!("unknown class {class_name}"))?;
        let rec_ty = self.shapes.named(ctx, class_name)?;
        let rec = TypedHandle::<RecordType>::from_handle(rec_ty, ctx)
            .map_err(|_| internal!("expected class record for {class_name}"))?;
        let field_defs: Vec<(String, TypeHandle)> = rec
            .deref(ctx)
            .fields()
            .map_err(|_| Diagnostic::internal("incomplete class record"))?
            .to_vec();
        let mut order = Vec::with_capacity(field_defs.len());
        let mut field_slots = Vec::with_capacity(class.fields.len());
        for field in &class.fields {
            let (index, field_ty) = rec
                .deref(ctx)
                .field(&field.name)
                .ok_or_else(|| internal!("missing field {} on {class_name}", field.name))?;
            field_slots.push((index, field_ty));
            if !(field.is_private && field.name.starts_with('#')) {
                order.push_unique(index as u32);
            }
        }
        if class.fields.len() == field_defs.len()
            && class.fields.iter().all(|f| f.init.is_none())
            && let Some(ctor) = &class.constructor
            && ctor.params.len() == class.fields.len()
            && ctor
                .params
                .iter()
                .all(|p| !p.is_rest && p.default.is_none())
            && ctor.body.len() == class.fields.len()
            && args.len() == class.fields.len()
            && ctor
                .body
                .iter()
                .zip(class.fields.iter().zip(&ctor.params))
                .all(|(stmt, (field, param))| {
                    matches!(
                        stmt,
                        Stmt::Expr(Expr::PropertySet { object, property, value, .. })
                            if matches!(object.as_ref(), Expr::This)
                                && property == &field.name
                                && matches!(value.as_ref(), Expr::LocalGet(id) if *id == param.id)
                    )
                })
        {
            let mut slots = vec![None; field_defs.len()];
            for (&(index, field_ty), expr) in field_slots.iter().zip(args) {
                let val = self.expression_with(ctx, expr, Some(field_ty))?;
                *slots.get_mut(index).ok_or_else(|| {
                    internal!("field slot {index} out of range on {class_name}")
                })? = Some(self.fit(ctx, val, field_ty)?);
            }
            let init_operands: Vec<Value> = slots
                .into_iter()
                .collect::<Option<_>>()
                .ok_or_else(|| internal!("incomplete direct-init fields on {class_name}"))?;
            let instance = self.emit_value(ctx, |ctx| {
                let alloc = AllocOp::new(ctx, rec, init_operands);
                alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(order));
                alloc
            });
            return Ok(instance);
        }
        let mut init_operands = Vec::with_capacity(field_defs.len());
        let mut initializers: Vec<_> = class
            .fields
            .iter()
            .zip(&field_slots)
            .map(|(field, &(_, field_ty))| (field.name.as_str(), field_ty, field.init.as_ref()))
            .collect();
        for (field, field_ty) in &field_defs {
            let initializer = initializers.iter_mut().find(|(name, _, _)| *name == field);
            let field_init = initializer.as_ref().and_then(|(_, _, init)| *init);
            let (val, used_init) = if admits_undefined(ctx, *field_ty) {
                (
                    self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, *field_ty)),
                    false,
                )
            } else if absence(ctx, *field_ty).is_some_and(|a| a.admits_null()) {
                (
                    self.emit_value(ctx, |ctx| NullOp::new(ctx, *field_ty)),
                    false,
                )
            } else {
                self.unassigned_field(ctx, class_name, field, *field_ty, field_init)?
            };
            if used_init && let Some((_, _, init)) = initializer {
                *init = None;
            }
            init_operands.push(val);
        }
        let instance = self.emit_value(ctx, |ctx| {
            let alloc = AllocOp::new(ctx, rec, init_operands);
            alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(order));
            alloc
        });
        let this_slot = self.entry_slot(ctx, rec_ty);
        self.store(ctx, this_slot, instance);
        if let Some(ctor) = &class.constructor {
            self.inline_class_callable(
                ctx,
                class_name,
                ctor,
                ClassArguments::Expressions(args),
                ClassReceiver::Constructor(this_slot, class, &initializers),
            )?;
        } else {
            for arg in args {
                self.effect(ctx, arg)?;
            }
            self.init_own_class_fields(ctx, class_name, class, this_slot, &initializers)?;
        }
        self.load(ctx, this_slot)
    }

    /// Initial placeholder value for a non-optional field before its constructor assignment.
    /// Requires a reference-free zero or empty value for `ty`, or a tagged-union placeholder.
    fn unassigned_field(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        field: &str,
        ty: TypeHandle,
        init: Option<&'s Expr>,
    ) -> Result<(Value, bool)> {
        if ty.deref(ctx).is::<FP64Type>() {
            return Ok((self.number(ctx, 0.0), false));
        }
        if perry_mir::typed::<IntegerType>(ctx, ty).is_some_and(|int| int.deref(ctx).width() == 1) {
            return Ok((self.boolean(ctx, false), false));
        }
        if let Some(zero) = self.zero_value_for_type(ctx, ty) {
            return Ok((zero, false));
        }
        if let Ok(variant) = TypedHandle::<perry_mir::VariantType>::from_handle(ty, ctx) {
            let payload_free = variant
                .deref(ctx)
                .cases()
                .iter()
                .find_map(|(tag, payload)| payload.is_none().then(|| tag.clone()));
            if let Some(tag) = payload_free {
                return Ok((
                    self.emit_value(ctx, |ctx| perry_mir::VariantNewOp::new(ctx, ty, &tag, None)),
                    false,
                ));
            }
        }
        let Some(init_expr) = init else {
            bail_unsupported!(
                "class {class_name} field {field} has no placeholder before its constructor assignment"
            );
        };
        let val = self.expression_with(ctx, init_expr, Some(ty))?;
        Ok((self.fit(ctx, val, ty)?, true))
    }

    fn init_own_class_fields(
        &mut self,
        ctx: &mut Context,
        class_name: &'s str,
        class: &'s perry_frontend::hir::ir::Class,
        this_slot: Value,
        initializers: &[(&'s str, TypeHandle, Option<&'s Expr>)],
    ) -> Result<()> {
        let saved_this = self.this_slot.replace(this_slot);
        let res = self.with_class_expansion(
            ctx,
            ClassExpansion::Fields(class),
            class_name,
            |this, ctx| {
                for &(name, field_ty, init) in initializers {
                    let Some(init_expr) = init else {
                        continue;
                    };
                    let val = this.expression_with(ctx, init_expr, Some(field_ty))?;
                    let val = this.fit(ctx, val, field_ty)?;
                    let target = this.load(ctx, this_slot)?;
                    this.emit(ctx, |ctx| {
                        FieldSetOp::new(ctx, target, name.to_owned(), val)
                    });
                }
                Ok(())
            },
        );
        self.this_slot = saved_this;
        res
    }

    pub(super) fn class_this(&mut self, ctx: &mut Context) -> Result<Value> {
        let slot = self.this_slot.ok_or_else(|| {
            Diagnostic::unsupported(
                "'this' is only supported inside class constructors, methods, and accessors",
            )
        })?;
        self.load(ctx, slot)
    }

    pub(super) fn is_proven_this_class(&self, ctx: &Context, class_name: &str) -> bool {
        self.this_slot
            .and_then(|slot| self.slot_type(ctx, slot).ok())
            .and_then(|ty| self.shapes.class_of_type(ctx, ty))
            == Some(class_name)
    }

    pub(super) fn class_static_method_call(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        method: &str,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let (owner_class, func) = self
            .shapes
            .class_static_method(class_name, method)
            .ok_or_else(|| {
                diag!(
                    codes::TYPE_MISMATCH,
                    "static method {class_name}.{method} not found"
                )
            })?;
        self.inline_class_callable(
            ctx,
            owner_class,
            func,
            ClassArguments::Expressions(args),
            ClassReceiver::Static,
        )
    }

    pub(super) fn class_static_field_get(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        field: &str,
    ) -> Result<Value> {
        let (id, _) = self.shapes.static_field(class_name, field).ok_or_else(|| {
            diag!(
                codes::TYPE_MISMATCH,
                "static field {class_name}.{field} not found"
            )
        })?;
        self.load_local(ctx, id)
    }

    pub(super) fn class_static_field_set(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        field: &str,
        value: &'s Expr,
    ) -> Result<Value> {
        let (id, _) = self.shapes.static_field(class_name, field).ok_or_else(|| {
            diag!(
                codes::TYPE_MISMATCH,
                "static field {class_name}.{field} not found"
            )
        })?;
        self.local_set(ctx, id, value)
    }

    pub(super) fn class_method_call(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        func: &'s Function,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        self.inline_class_callable(
            ctx,
            owner_class,
            func,
            ClassArguments::Expressions(args),
            ClassReceiver::Instance(receiver),
        )
    }

    pub(super) fn class_getter_call(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        getter: &'s Function,
    ) -> Result<Value> {
        self.inline_class_callable(
            ctx,
            owner_class,
            getter,
            ClassArguments::Expressions(&[]),
            ClassReceiver::Instance(receiver),
        )?
        .ok_or_else(|| Diagnostic::new(codes::TYPE_MISMATCH, "class getter produced no value"))
    }

    pub(super) fn class_setter_call(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        setter: &'s Function,
        value: &'s Expr,
    ) -> Result<Value> {
        let [param] = setter.params.as_slice() else {
            bail_internal!("emitted setter requires one parameter");
        };
        let param_ty = self.shapes.value_type(ctx, &param.ty)?;
        let val = self.expression_with(ctx, value, Some(param_ty))?;
        let val = self.fit(ctx, val, param_ty)?;
        self.class_setter_call_value(ctx, receiver, owner_class, setter, val)
    }

    pub(super) fn class_setter_call_value(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        setter: &'s Function,
        val: Value,
    ) -> Result<Value> {
        self.inline_class_callable(
            ctx,
            owner_class,
            setter,
            ClassArguments::Evaluated(std::slice::from_ref(&val)),
            ClassReceiver::Instance(receiver),
        )?;
        Ok(val)
    }

    fn inline_class_callable(
        &mut self,
        ctx: &mut Context,
        owner_class: &'s str,
        func: &'s Function,
        args: ClassArguments<'_, 's>,
        receiver: ClassReceiver<'_, 's>,
    ) -> Result<Option<Value>> {
        ensure_unsupported!(
            !func.is_async && !func.is_generator,
            "async or generator class methods are not supported yet"
        );
        let mut args = args;
        let mut next_arg = |this: &mut Self,
                            ctx: &mut Context,
                            expected,
                            default: Option<&Expr>| {
            match &mut args {
                ClassArguments::Evaluated(values) => {
                    let value = values.first().copied();
                    *values = values.get(1..).unwrap_or_default();
                    Ok(value)
                }
                ClassArguments::Expressions(expressions) => {
                    let Some((expr, rest)) = expressions.split_first() else {
                        return Ok(None);
                    };
                    *expressions = rest;
                    if matches!(expr, Expr::Undefined) && default.is_some() {
                        Ok(None)
                    } else {
                        this.expression_with(ctx, expr, expected).map(Some)
                    }
                }
            }
        };
        let mut evaluated_args = Vec::with_capacity(func.params.len());
        for param in &func.params {
            let param_ty = self.shapes.value_type(ctx, &param.ty)?;
            let value = if param.is_rest {
                let element = Self::rest_element_type(ctx, param_ty)?;
                let mut items = Vec::new();
                while let Some(value) = next_arg(self, ctx, Some(element), None)? {
                    items.push(self.fit(ctx, value, element)?);
                }
                Some(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, items)))
            } else {
                next_arg(self, ctx, Some(param_ty), param.default.as_ref())?
            };
            evaluated_args.push((param_ty, value));
            if param.is_rest {
                break;
            }
        }
        while next_arg(self, ctx, None, None)?.is_some() {}

        self.with_class_expansion(
            ctx,
            ClassExpansion::Callable(func),
            owner_class,
            |this, ctx| {
                let this_slot = match receiver {
                    ClassReceiver::Static => None,
                    ClassReceiver::Instance(receiver) => {
                        let slot = this.entry_slot(ctx, receiver.get_type(ctx));
                        this.store(ctx, slot, receiver);
                        Some(slot)
                    }
                    ClassReceiver::Constructor(slot, class, initializers) => {
                        this.init_own_class_fields(ctx, owner_class, class, slot, initializers)?;
                        Some(slot)
                    }
                };

                let slot = this.with_inline_scope(ctx, this_slot, |this, ctx| {
                    for (param, (param_ty, eval_arg)) in func.params.iter().zip(evaluated_args) {
                        this.bind_parameter(ctx, param, eval_arg, BindingType::plain(param_ty))?;
                    }

                    let result_type = if matches!(receiver, ClassReceiver::Constructor(..))
                        || matches!(func.return_type, HirType::Void)
                    {
                        InlineResult::Void
                    } else {
                        InlineResult::Declared(this.shapes.value_type(ctx, &func.return_type)?)
                    };
                    this.inline_body(ctx, &func.body, result_type)
                })?;
                if let ClassReceiver::Constructor(slot, _, _) = receiver {
                    return this.load(ctx, slot).map(Some);
                }
                match slot {
                    Some(slot) => this.load(ctx, slot).map(Some),
                    None => Ok(None),
                }
            },
        )
    }
}
