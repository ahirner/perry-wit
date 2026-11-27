//! User-defined class construction, field initialization, methods, accessors, and inheritance.
use super::*;

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn class_new(
        &mut self,
        ctx: &mut Context,
        class: &'s crate::source::InstanceClass<'s>,
        args: &'s [Expr],
        arguments: Option<crate::engine::types::Ty>,
    ) -> Result<Value> {
        self.invoke_arguments(
            ctx,
            self.signature(class.factory()?)?,
            std::iter::empty(),
            Arguments::Constructor(args),
            arguments,
            None,
        )?
        .ok_or_else(|| internal!("class constructor has no result"))
    }

    pub(super) fn construct(
        &mut self,
        ctx: &mut Context,
        class: &'s crate::source::InstanceClass<'s>,
        arguments: Option<&[Value]>,
    ) -> Result<Value> {
        let class_name = class.name.as_str();
        let rec_ty = self.shapes.resolved_binding(class.ty)?.carrier;
        let rec = TypedHandle::<RecordType>::from_handle(rec_ty, ctx)
            .map_err(|_| internal!("expected class record for {class_name}"))?;
        let layout = self
            .shapes
            .class_records
            .get(&rec_ty)
            .ok_or_else(|| internal!("missing layout for class {class_name}"))?
            .clone();
        let mut operands = Vec::with_capacity(layout.fields.len());
        let mut initializers = Vec::new();
        for (i, (&(field, _), &ty)) in class.fields.iter().zip(&layout.fields).enumerate() {
            let value = if let Some(args) = arguments {
                self.fit(
                    ctx,
                    *args
                        .get(i)
                        .ok_or_else(|| internal!("missing direct field argument"))?,
                    ty,
                )?
            } else if let Some(value) = self.field_placeholder(ctx, ty) {
                if let Some(init) = &field.init {
                    initializers.push((field.name.as_str(), ty, init));
                }
                value
            } else {
                let init = field.init.as_ref().ok_or_else(|| unsupported!(
                    "class {class_name} field {} has no placeholder before its constructor assignment", field.name
                ))?;
                self.field_initializer(ctx, class, init, ty)?
            };
            operands.push(value);
        }
        let instance = self.emit_value(ctx, |ctx| {
            let alloc = AllocOp::new(ctx, rec, operands);
            alloc.set_attr_perry_field_order(ctx, layout.order.clone());
            alloc
        });
        if arguments.is_none() {
            let slot = self.initialized_local(ctx, instance).slot;
            self.frame.this_slot = Some(slot);
            for (name, ty, init) in initializers {
                let val = self.field_initializer(ctx, class, init, ty)?;
                self.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, instance, name.to_owned(), val)
                });
            }
        }
        Ok(instance)
    }

    fn field_placeholder(&mut self, ctx: &mut Context, ty: TypeHandle) -> Option<Value> {
        if let Some(states) = absence(ctx, ty) {
            return Some(if states.admits_undefined() {
                self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
            } else {
                self.emit_value(ctx, |ctx| NullOp::new(ctx, ty))
            });
        }
        if ty.deref(ctx).is::<FP64Type>() {
            return Some(self.number(ctx, 0.0));
        }
        if perry_mir::typed::<IntegerType>(ctx, ty).is_some_and(|int| int.deref(ctx).width() == 1) {
            return Some(self.boolean(ctx, false));
        }
        self.zero_value_for_type(ctx, ty).or_else(|| {
            let variant = perry_mir::typed::<perry_mir::VariantType>(ctx, ty)?;
            let (tag, _) = variant
                .deref(ctx)
                .cases()
                .iter()
                .find(|(_, payload)| payload.is_none())?
                .clone();
            Some(self.emit_value(ctx, |ctx| perry_mir::VariantNewOp::new(ctx, ty, &tag, None)))
        })
    }

    fn field_initializer(
        &mut self,
        ctx: &mut Context,
        class: &crate::source::InstanceClass,
        expr: &'s Expr,
        ty: TypeHandle,
    ) -> Result<Value> {
        let owner = class
            .initializer
            .map_or(BodyOwner::Initializer, BodyOwner::Function);
        let saved = std::mem::replace(&mut self.frame.owner, owner);
        let result = self.expression_with(ctx, expr, Some(ty));
        self.frame.owner = saved;
        self.fit(ctx, result?, ty)
    }

    pub(super) fn class_this(&mut self, ctx: &mut Context) -> Result<Value> {
        let slot = self.frame.this_slot.ok_or_else(|| {
            Diagnostic::unsupported(
                "'this' is only supported inside class constructors, methods, and accessors",
            )
        })?;
        self.load(ctx, slot)
    }

    pub(super) fn is_proven_this_class(&self, ctx: &Context, class_name: &str) -> bool {
        self.frame
            .this_slot
            .and_then(|slot| self.slot_type(ctx, slot).ok())
            .and_then(|ty| self.shapes.class_of_type(ctx, ty))
            == Some(class_name)
    }
}
