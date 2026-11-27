//! User-defined class construction, field initialization, methods, accessors, and inheritance.
use super::*;

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn class_new(
        &mut self,
        ctx: &mut Context,
        class: &'s crate::source::InstanceClass<'s>,
        args: &'s [Expr],
    ) -> Result<Value> {
        self.invoke_arguments(
            ctx,
            self.signature(class.factory()?)?,
            std::iter::empty(),
            Arguments::Plain(args),
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
        let fields: Vec<_> = crate::shapes::ClassShape::Instance(class)
            .fields()
            .zip(rec.deref(ctx).fields().to_diag(ctx)?)
            .map(|((field, _), (_, ty))| (field, *ty))
            .collect();
        let mut operands = Vec::with_capacity(fields.len());
        let mut order = Vec::new();
        for (index, &(field, ty)) in fields.iter().enumerate() {
            let value = if let Some(args) = arguments {
                let value = args
                    .get(index)
                    .ok_or_else(|| internal!("missing direct field argument"))?;
                if !hir_compat::private_field(field) {
                    order.push(index as u32);
                }
                self.fit(ctx, *value, ty)?
            } else {
                self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty))
            };
            operands.push(value);
        }
        let instance = self.emit_value(ctx, |ctx| {
            let alloc = AllocOp::new(ctx, rec, operands);
            alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(order));
            alloc
        });
        if arguments.is_none() {
            self.frame.this_slot = Some(self.initialized_local(ctx, instance).slot);
            self.frame.owner = class
                .initializer
                .map_or(BodyOwner::Initializer, BodyOwner::Function);
            for (index, &(field, ty)) in fields.iter().enumerate() {
                if field.origin.is_definition() {
                    let init = match &field.init {
                        Some(expr) => expressions::Assigned::Expression(expr),
                        None => expressions::Assigned::Value(
                            self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)),
                        ),
                    };
                    expressions::Member::Field(instance, &field.name, index, ty)
                        .write(self, ctx, init)?;
                }
            }
        }
        Ok(instance)
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
