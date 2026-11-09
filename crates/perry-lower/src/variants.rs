//! Closed union construction and tag-based payload selection.
//!
//! Source guards (`typeof x === "number"`, `x.tag === "a"`, tracked by
//! [`perry_frontend::narrowing::Guards`]) select the payload type a read lowers to; the payload
//! read checks the tag at runtime.
use super::*;
use perry_frontend::narrowing::{Discriminant, path};
use perry_mir::{VariantNewOp, VariantPayloadOp, VariantTagOp, VariantType};

impl<'s> FunctionBuilder<'s> {
    /// After `local = expr`, lowered to `assigned`, the local holds the alternative of the
    /// assigned value's own type or the case of a variant literal, as TypeScript narrows on
    /// assignment.
    pub(super) fn assignment_narrowing(
        &mut self,
        ctx: &Context,
        id: LocalId,
        (expr, assigned): (&'s Expr, Value),
        carrier: TypeHandle,
    ) {
        if crate::value_unions::is_value_union(ctx, present_type(ctx, carrier))
            && let Some(tag) = perry_mir::value_tag(ctx, assigned.get_type(ctx))
        {
            self.variant_guards.assign_as(id, Discriminant::TypeOf, tag);
        } else if let Some(tag) = self.literal_tag(ctx, expr, assigned) {
            self.variant_guards.assign_as(id, Discriminant::Tag, tag);
        } else {
            self.variant_guards.assign(id);
        }
    }

    /// The tag of `expr` when it is a variant literal, lowered to `value`.
    fn literal_tag(&self, ctx: &Context, expr: &'s Expr, value: Value) -> Option<&'s str> {
        value
            .defining_op()
            .filter(|&op| Operation::is_op::<VariantNewOp>(op, ctx))?;
        let Expr::New {
            class_name, args, ..
        } = expr
        else {
            return None;
        };
        let fields = self.shapes.literal_fields(class_name)?;
        match args.get(fields.iter().position(|&name| name == "tag")?)? {
            Expr::String(tag) => Some(tag),
            _ => None,
        }
    }

    pub(super) fn narrow_value(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        value: Value,
    ) -> Result<Value> {
        let ty = present_type(ctx, value.get_type(ctx));
        if !crate::value_unions::is_value_union(ctx, ty) {
            return Ok(value);
        }
        let Some(path) = path(expr) else {
            return Ok(value);
        };
        let cases = perry_mir::variant_cases(ctx, ty)?;
        let mut possible = cases.iter().filter(|(tag, _)| {
            self.variant_guards
                .tags(&path, Discriminant::TypeOf)
                .all(|(guard, equal)| (tag == guard) == equal)
        });
        let Some((tag, Some(result))) = possible.next() else {
            return Ok(value);
        };
        if possible.next().is_some() {
            return Ok(value);
        }
        let result = *result;
        let asserted = self
            .variant_guards
            .tags(&path, Discriminant::TypeOf)
            .any(|(guard, equal)| equal && guard == tag);
        // Negative typeof guards alone do not remove null or undefined.
        if self.is_optional(ctx, value)
            && (!asserted
                || (tag == "object"
                    && absence(ctx, value.get_type(ctx)).is_some_and(|state| state.admits_null())))
        {
            return Ok(value);
        }
        let value = self.defined(ctx, value)?;
        Ok(self.emit_value(ctx, |ctx| VariantPayloadOp::new(ctx, value, result)))
    }

    pub(super) fn variant_member(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        value: Value,
        field: &str,
    ) -> Result<Value> {
        if field == "tag" {
            return Ok(self.emit_value(ctx, |ctx| VariantTagOp::new(ctx, value)));
        }
        ensure!(field == "val", "variants have tag and val only");
        let variant = TypedHandle::<VariantType>::from_handle(value.get_type(ctx), ctx)?;
        let path = path(object);
        let mut reachable = false;
        let mut payload = None;
        let mut absent = false;
        for (tag, ty) in variant.deref(ctx).cases() {
            if let Some(path) = &path
                && !self
                    .variant_guards
                    .tags(path, Discriminant::Tag)
                    .all(|(guard, equal)| (tag == guard) == equal)
            {
                continue;
            }
            reachable = true;
            match *ty {
                Some(ty) => {
                    ensure!(
                        payload.is_none_or(|other| other == ty),
                        "narrow a heterogeneous variant by its tag before reading val"
                    );
                    payload = Some(ty);
                }
                None => absent = true,
            }
        }
        ensure!(reachable, "variant payload access has no reachable case");
        let result = match payload {
            Some(ty) if absent => perry_mir::OptionalType::get(ctx, ty).into(),
            Some(ty) => ty,
            None => self.shapes.value_type(ctx, &HirType::Void)?,
        };
        Ok(self.emit_value(ctx, |ctx| VariantPayloadOp::new(ctx, value, result)))
    }

    pub(super) fn variant_literal(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        names: &[&str],
        args: &'s [Expr],
    ) -> Result<Value> {
        ensure!(
            names.iter().all(|&n| n == "tag" || n == "val"),
            "variants have tag and val only"
        );
        let tag = names
            .iter()
            .position(|&n| n == "tag")
            .context("variant literal needs a tag")?;
        let Expr::String(tag) = &args[tag] else {
            bail!("variant construction needs a literal tag");
        };
        let variant = TypedHandle::<VariantType>::from_handle(ty, ctx)?;
        let payload = variant
            .deref(ctx)
            .cases()
            .iter()
            .find(|(name, _)| name == tag)
            .map(|(_, payload)| *payload)
            .context("literal tag is not a case of this variant")?;
        let argument = names.iter().position(|&n| n == "val");
        let value = match (argument, payload) {
            (Some(index), Some(payload)) => {
                let value = self.expression_with(ctx, &args[index], Some(payload))?;
                Some(self.fit(ctx, value, payload)?)
            }
            (None, None) => None,
            _ => bail!("variant val must match its active case"),
        };
        Ok(self.emit_value(ctx, |ctx| VariantNewOp::new(ctx, ty, tag, value)))
    }
}
