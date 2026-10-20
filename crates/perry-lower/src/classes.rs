//! User-defined class construction, field initialization, methods, accessors, and inheritance.
use super::*;
use crate::{builder::LocalBinding, inline::InlineResult, shapes::TypeId};
use perry_mir::OptionalType;

enum ClassArguments<'a, 's> {
    Expressions(&'s [Expr]),
    Evaluated(&'a [Value]),
}

enum ClassReceiver {
    Static,
    Instance(Value),
    Constructor(Value),
}

pub(super) enum ClassExpansion<'s> {
    Callable(&'s Function),
    Fields(&'s perry_hir::ir::Class),
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
                    bail!(
                        "recursive inline class call to {owner_class}.{} is not supported yet",
                        func.name
                    )
                }
                ClassExpansion::Fields(_) => {
                    bail!(
                        "recursive inline field initialization of class {owner_class} is not supported yet"
                    )
                }
            }
        }
        self.class_expansions.push(expansion);
        let result = lower(self, ctx);
        self.class_expansions.pop();
        result
    }

    /// `new ClassName<type_args>(args)` for a user-defined class.
    pub(super) fn class_new(
        &mut self,
        ctx: &mut Context,
        class_name: &'s str,
        args: &'s [Expr],
        type_args: &'s [HirType],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let (&class_name, &(_, class)) = self
            .shapes
            .classes
            .get_key_value(class_name)
            .with_context(|| format!("unknown class {class_name}"))?;
        let mut pre_evaluated: Option<Vec<Value>> = None;
        let (rec_ty, effective_args) = if !type_args.is_empty() {
            let ty = self.shapes.generic(ctx, class_name, type_args)?;
            let effective = self
                .shapes
                .class_of_type(ctx, ty)
                .map(|(_, a)| a)
                .unwrap_or_else(|| TypeId::intern_slice(ctx, type_args));
            (ty, effective)
        } else if class.type_params.is_empty() {
            (self.shapes.named(ctx, class_name)?, Vec::new())
        } else if let Some(exp) = expected.map(|ty| present_type(ctx, ty))
            && let Some((c, mapped)) = self.shapes.class_of_type(ctx, exp)
            && c == class_name
        {
            (exp, mapped)
        } else {
            let mut vals = Vec::with_capacity(args.len());
            for arg in args {
                vals.push(self.expression(ctx, arg)?);
            }
            let mut arg_tys = Vec::with_capacity(class.type_params.len());
            for tp in &class.type_params {
                let inferred = class.constructor.as_ref().and_then(|ctor| {
                    ctor.params.iter().enumerate().find_map(|(idx, p)| {
                        if matches!(&p.ty, HirType::TypeVar(n) | HirType::Named(n) if n == &tp.name)
                            && let Some(&val) = vals.get(idx)
                        {
                            mir_type_to_hir(ctx, self.shapes, val.get_type(ctx))
                        } else {
                            None
                        }
                    })
                });
                let arg_ty = inferred.unwrap_or_else(|| {
                    tp.default
                        .as_deref()
                        .or(tp.constraint.as_deref())
                        .unwrap_or(&HirType::Any)
                        .clone()
                });
                arg_tys.push(arg_ty);
            }
            pre_evaluated = Some(vals);
            let ty = self.shapes.generic(ctx, class_name, &arg_tys)?;
            let effective = self
                .shapes
                .class_of_type(ctx, ty)
                .map(|(_, a)| a)
                .unwrap_or_else(|| TypeId::intern_slice(ctx, &arg_tys));
            (ty, effective)
        };

        let rec = TypedHandle::<RecordType>::from_handle(rec_ty, ctx)
            .map_err(|_| anyhow!("expected class record for {class_name}"))?;
        let field_defs: Vec<(String, TypeHandle)> = rec
            .deref(ctx)
            .fields()
            .context("incomplete class record")?
            .to_vec();
        let mut init_operands = Vec::with_capacity(field_defs.len());
        for (field, field_ty) in &field_defs {
            let val = if admits_undefined(ctx, *field_ty) {
                self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, *field_ty))
            } else if absence(ctx, *field_ty).is_some_and(|a| a.admits_null()) {
                self.emit_value(ctx, |ctx| NullOp::new(ctx, *field_ty))
            } else {
                self.unassigned_field(ctx, class_name, field, *field_ty)?
            };
            init_operands.push(val);
        }
        let mut order = Vec::with_capacity(field_defs.len());
        for field in &class.fields {
            let (index, _) = rec
                .deref(ctx)
                .field(&field.name)
                .with_context(|| format!("missing field {} on {class_name}", field.name))?;
            if !order.contains(&(index as u32)) {
                order.push(index as u32);
            }
        }
        let instance = self.emit_value(ctx, |ctx| {
            let alloc = AllocOp::new(ctx, rec, init_operands);
            alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(order));
            alloc
        });
        let this_slot = self.entry_slot(ctx, rec_ty);
        self.store(ctx, this_slot, instance);
        if let Some(ctor) = &class.constructor {
            let args = match pre_evaluated.as_deref() {
                Some(values) => ClassArguments::Evaluated(values),
                None => ClassArguments::Expressions(args),
            };
            self.inline_class_callable(
                ctx,
                class_name,
                &effective_args,
                ctor,
                args,
                ClassReceiver::Constructor(this_slot),
            )?;
        } else {
            if pre_evaluated.is_none() {
                for arg in args {
                    self.expression(ctx, arg)?;
                }
            }
            self.init_own_class_fields(ctx, class_name, this_slot)?;
        }
        self.load(ctx, this_slot)
    }

    /// The value a non-optional field holds between allocation and its constructor assignment.
    /// TypeScript's definite assignment means it is never observed; types without a
    /// reference-free value fail loudly instead of guessing.
    fn unassigned_field(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        field: &str,
        ty: TypeHandle,
    ) -> Result<Value> {
        if ty.deref(ctx).is::<FP64Type>() {
            Ok(self.number(ctx, 0.0))
        } else if TypedHandle::<IntegerType>::from_handle(ty, ctx)
            .is_ok_and(|int| int.deref(ctx).width() == 1)
        {
            Ok(self.boolean(ctx, false))
        } else if let Some(zero) = self.zero_value_for_type(ctx, ty) {
            Ok(zero)
        } else {
            bail!(
                "class {class_name} field {field} has no placeholder before its constructor assignment"
            )
        }
    }

    fn init_own_class_fields(
        &mut self,
        ctx: &mut Context,
        class_name: &'s str,
        this_slot: Value,
    ) -> Result<()> {
        let (&class_name, &(_, class)) = self
            .shapes
            .classes
            .get_key_value(class_name)
            .with_context(|| format!("unknown class {class_name}"))?;
        let rec_val = self.load(ctx, this_slot)?;
        let rec = TypedHandle::<RecordType>::from_handle(rec_val.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected class record"))?;
        let saved_this = self.this_slot.replace(this_slot);
        let res = self.with_class_expansion(
            ctx,
            ClassExpansion::Fields(class),
            class_name,
            |this, ctx| {
                for field in &class.fields {
                    let Some(init_expr) = &field.init else {
                        continue;
                    };
                    let (_, field_ty) = rec
                        .deref(ctx)
                        .field(&field.name)
                        .with_context(|| format!("missing field {} on {class_name}", field.name))?;
                    let val = this.expression_with(ctx, init_expr, Some(field_ty))?;
                    let val = this.fit(ctx, val, field_ty)?;
                    let target = this.load(ctx, this_slot)?;
                    this.emit(ctx, |ctx| {
                        FieldSetOp::new(ctx, target, field.name.clone(), val)
                    });
                }
                Ok(())
            },
        );
        self.this_slot = saved_this;
        res
    }

    pub(super) fn class_this(&mut self, ctx: &mut Context) -> Result<Value> {
        let slot = self.this_slot.context(
            "'this' is only supported inside class constructors, methods, and accessors",
        )?;
        self.load(ctx, slot)
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
            .with_context(|| format!("static method {class_name}.{method} not found"))?;
        self.inline_class_callable(
            ctx,
            owner_class,
            &[],
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
        let (id, _) = self
            .shapes
            .static_field(class_name, field)
            .with_context(|| format!("static field {class_name}.{field} not found"))?;
        self.load_local(ctx, id)
    }

    pub(super) fn class_static_field_set(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        field: &str,
        value: &'s Expr,
    ) -> Result<Value> {
        let (id, _) = self
            .shapes
            .static_field(class_name, field)
            .with_context(|| format!("static field {class_name}.{field} not found"))?;
        self.local_set(ctx, id, value)
    }

    pub(super) fn class_method_call(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        type_args: &[TypeId],
        func: &'s Function,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let slot = self.entry_slot(ctx, receiver.get_type(ctx));
        self.store(ctx, slot, receiver);
        self.inline_class_callable(
            ctx,
            owner_class,
            type_args,
            func,
            ClassArguments::Expressions(args),
            ClassReceiver::Instance(slot),
        )
    }

    pub(super) fn class_getter_call(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        type_args: &[TypeId],
        getter: &'s Function,
    ) -> Result<Value> {
        let slot = self.entry_slot(ctx, receiver.get_type(ctx));
        self.store(ctx, slot, receiver);
        self.inline_class_callable(
            ctx,
            owner_class,
            type_args,
            getter,
            ClassArguments::Expressions(&[]),
            ClassReceiver::Instance(slot),
        )?
        .context("class getter produced no value")
    }

    pub(super) fn class_setter_call(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        type_args: &[TypeId],
        setter: &'s Function,
        value: &'s Expr,
    ) -> Result<Value> {
        let param_ty = setter
            .params
            .first()
            .map(|p| &p.ty)
            .filter(|ty| !matches!(ty, HirType::Any | HirType::Unknown))
            .and_then(|ty| {
                self.shapes
                    .class_value_type(ctx, owner_class, type_args, ty)
                    .ok()
            });
        let val = self.expression_with(ctx, value, param_ty)?;
        let val = match param_ty {
            Some(ty) => self.fit(ctx, val, ty)?,
            None => val,
        };
        self.class_setter_call_value(ctx, receiver, owner_class, type_args, setter, val)
    }

    pub(super) fn class_setter_call_value(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        owner_class: &'s str,
        type_args: &[TypeId],
        setter: &'s Function,
        val: Value,
    ) -> Result<Value> {
        let slot = self.entry_slot(ctx, receiver.get_type(ctx));
        self.store(ctx, slot, receiver);
        self.inline_class_callable(
            ctx,
            owner_class,
            type_args,
            setter,
            ClassArguments::Evaluated(std::slice::from_ref(&val)),
            ClassReceiver::Instance(slot),
        )?;
        Ok(val)
    }

    fn inline_class_callable(
        &mut self,
        ctx: &mut Context,
        owner_class: &'s str,
        type_args: &[TypeId],
        func: &'s Function,
        args: ClassArguments<'_, 's>,
        receiver: ClassReceiver,
    ) -> Result<Option<Value>> {
        ensure!(
            !func.is_async && !func.is_generator,
            "async or generator class methods are not supported yet"
        );
        // Evaluate caller arguments in the caller's scope before switching `locals` and `this_slot`.
        enum EvalArg {
            Present(Value),
            Omitted,
            Rest(Value),
        }
        let mut evaluated_args = Vec::with_capacity(func.params.len());
        for (i, param) in func.params.iter().enumerate() {
            if param.is_rest {
                let rest_ty =
                    self.shapes
                        .class_value_type(ctx, owner_class, type_args, &param.ty)?;
                let elem_ty = TypedHandle::<ArrayType>::from_handle(rest_ty, ctx)
                    .map_err(|_| anyhow!("rest parameter must have array type"))?
                    .deref(ctx)
                    .element();
                let mut elems = Vec::new();
                match args {
                    ClassArguments::Evaluated(values) => {
                        for &v in values.iter().skip(i) {
                            elems.push(self.fit(ctx, v, elem_ty)?);
                        }
                    }
                    ClassArguments::Expressions(expressions) => {
                        for arg in expressions.iter().skip(i) {
                            let v = self.expression_with(ctx, arg, Some(elem_ty))?;
                            elems.push(self.fit(ctx, v, elem_ty)?);
                        }
                    }
                }
                let arr = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, elem_ty, elems));
                evaluated_args.push((Some(rest_ty), EvalArg::Rest(arr)));
                break;
            }
            let param_ty = if matches!(param.ty, HirType::Any | HirType::Unknown) {
                None
            } else {
                Some(
                    self.shapes
                        .class_value_type(ctx, owner_class, type_args, &param.ty)?,
                )
            };
            let evaluated = match args {
                ClassArguments::Evaluated(values) => values
                    .get(i)
                    .copied()
                    .map_or(EvalArg::Omitted, EvalArg::Present),
                ClassArguments::Expressions(expressions) => match expressions.get(i) {
                    Some(Expr::Undefined) if param.default.is_some() => EvalArg::Omitted,
                    Some(expr) => EvalArg::Present(self.expression_with(ctx, expr, param_ty)?),
                    None => EvalArg::Omitted,
                },
            };
            evaluated_args.push((param_ty, evaluated));
        }
        // Any excess arguments beyond declared params are still evaluated for side effects.
        if let ClassArguments::Expressions(args) = args
            && !func.params.iter().any(|p| p.is_rest)
            && args.len() > func.params.len()
        {
            for arg in &args[func.params.len()..] {
                self.expression(ctx, arg)?;
            }
        }

        self.with_class_expansion(
            ctx,
            ClassExpansion::Callable(func),
            owner_class,
            |this, ctx| {
                if let ClassReceiver::Constructor(slot) = receiver {
                    this.init_own_class_fields(ctx, owner_class, slot)?;
                }
                let saved_locals = this.locals.scope();
                let saved_closures = this.closures.scope();
                let saved_this = this.this_slot.take();
                let guards = this.variant_guards.len();
                let finallies = std::mem::take(&mut this.finallies);
                let controls = std::mem::take(&mut this.controls);
                this.this_slot = match receiver {
                    ClassReceiver::Static => None,
                    ClassReceiver::Instance(slot) | ClassReceiver::Constructor(slot) => Some(slot),
                };

                let result = (|| {
                    for (param, (param_ty, eval_arg)) in func.params.iter().zip(evaluated_args) {
                        match eval_arg {
                            EvalArg::Rest(arr) => {
                                let slot = this.entry_slot(ctx, arr.get_type(ctx));
                                this.store(ctx, slot, arr);
                                this.locals.insert(param.id, LocalBinding::plain(slot));
                            }
                            EvalArg::Omitted => {
                                let val = if let Some(def_expr) = &param.default {
                                    let def_val = this.expression_with(ctx, def_expr, param_ty)?;
                                    match param_ty {
                                        Some(ty) => this.fit(ctx, def_val, ty)?,
                                        None => def_val,
                                    }
                                } else if let Some(ty) = param_ty {
                                    let ty = if admits_undefined(ctx, ty) {
                                        ty
                                    } else {
                                        OptionalType::get(ctx, ty).into()
                                    };
                                    this.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty))
                                } else {
                                    let f64_ty = FP64Type::get(ctx).into();
                                    let opt_ty = OptionalType::get(ctx, f64_ty).into();
                                    this.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, opt_ty))
                                };
                                let slot = this.entry_slot(ctx, val.get_type(ctx));
                                this.store(ctx, slot, val);
                                this.locals.insert(param.id, LocalBinding::plain(slot));
                            }
                            EvalArg::Present(value) => {
                                this.bind_inline_parameter(ctx, param, value, param_ty)?;
                            }
                        }
                    }

                    let result_type = if matches!(receiver, ClassReceiver::Constructor(_))
                        || matches!(func.return_type, HirType::Void)
                    {
                        InlineResult::Void
                    } else if matches!(func.return_type, HirType::Any | HirType::Unknown) {
                        InlineResult::Inferred
                    } else {
                        this.shapes
                            .class_value_type(ctx, owner_class, type_args, &func.return_type)
                            .ok()
                            .map_or(InlineResult::Inferred, InlineResult::Declared)
                    };
                    this.inline_body(ctx, &func.body, &func.params, result_type)
                })();
                this.locals.restore(saved_locals);
                this.closures.restore(saved_closures);
                this.this_slot = saved_this;
                this.variant_guards.truncate(guards);
                this.finallies = finallies;
                this.controls = controls;
                let slot = result?;
                if let ClassReceiver::Constructor(slot) = receiver {
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

fn mir_type_to_hir(ctx: &Context, shapes: &Shapes<'_>, ty: TypeHandle) -> Option<HirType> {
    if let Some((inner, states)) = optional_parts(ctx, ty) {
        let mut members = vec![mir_type_to_hir(ctx, shapes, inner)?];
        if states.admits_null() {
            members.push(HirType::Null);
        }
        if states.admits_undefined() {
            members.push(HirType::Void);
        }
        return Some(HirType::Union(members));
    }
    if let Some((c_name, c_args)) = shapes.class_of_type(ctx, ty) {
        return Some(if c_args.is_empty() {
            HirType::Named(c_name.to_string())
        } else {
            HirType::Generic {
                base: c_name.to_string(),
                type_args: c_args.into_iter().map(|id| id.hir(ctx).clone()).collect(),
            }
        });
    }
    if let Ok(arr) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
        let elem = mir_type_to_hir(ctx, shapes, arr.deref(ctx).element())?;
        return Some(HirType::Array(Box::new(elem)));
    }
    if ty == FP64Type::get(ctx).into() {
        return Some(HirType::Number);
    }
    if let Some(int_ty) = ty.deref(ctx).downcast_ref::<IntegerType>() {
        return Some(match int_ty.width() {
            1 => HirType::Boolean,
            64 => HirType::BigInt,
            _ => HirType::Number,
        });
    }
    if ty == BigIntType::get(ctx).into() {
        return Some(HirType::BigInt);
    }
    if ty == StringType::get(ctx).into() {
        return Some(HirType::String);
    }
    None
}
