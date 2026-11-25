//! Closed union construction and tag-based payload selection.
//!
//! Source guards (`typeof x === "number"`, `x.tag === "a"`, tracked by
//! [`crate::narrowing::Guards`]) select the payload type a read lowers to; the payload
//! read checks the tag at runtime.
use super::*;
use crate::narrowing::{Discriminant, path};
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
        if crate::lower::value_unions::is_value_union(ctx, present_type(ctx, carrier))
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
        let tag = match expr {
            Expr::New {
                class_name, args, ..
            } => {
                let fields = self.shapes.literal_fields(class_name)?;
                args.get(fields.iter().position(|&name| name == "tag")?)?
            }
            Expr::Object(fields) => &fields.iter().find(|(name, _)| name == "tag")?.1,
            _ => return None,
        };
        match tag {
            Expr::String(tag) => Some(tag),
            _ => None,
        }
    }

    pub(super) fn narrow_value(
        &mut self,
        ctx: &mut Context,
        path: impl FnOnce() -> Option<crate::narrowing::Path<'s>>,
        value: Value,
    ) -> Result<Value> {
        let ty = present_type(ctx, value.get_type(ctx));
        if !crate::lower::value_unions::is_value_union(ctx, ty) {
            return Ok(value);
        }
        let Some(path) = path() else {
            return Ok(value);
        };
        let cases = perry_mir::variant_cases(ctx, ty).to_diag(ctx)?;
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
        ensure_diag!(
            field == "val",
            codes::TYPE_MISMATCH,
            "variants have tag and val only"
        );
        let variant =
            TypedHandle::<VariantType>::from_handle(value.get_type(ctx), ctx).to_diag(ctx)?;
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
                    ensure_diag!(
                        payload.is_none_or(|other| other == ty),
                        codes::TYPE_MISMATCH,
                        "narrow a heterogeneous variant by its tag before reading val"
                    );
                    payload = Some(ty);
                }
                None => absent = true,
            }
        }
        ensure_diag!(
            reachable,
            codes::TYPE_MISMATCH,
            "variant payload access has no reachable case"
        );
        let result = match payload {
            Some(ty) if absent => perry_mir::OptionalType::get(ctx, ty).into(),
            Some(ty) => ty,
            None => void_type(ctx),
        };
        Ok(self.emit_value(ctx, |ctx| VariantPayloadOp::new(ctx, value, result)))
    }

    pub(super) fn variant_literal(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        fields: &[(&str, &'s Expr)],
    ) -> Result<Value> {
        ensure_diag!(
            fields
                .iter()
                .all(|&(n, e)| self.erased_operand(e) || n == "tag" || n == "val"),
            codes::TYPE_MISMATCH,
            "variants have tag and val only"
        );
        let tag_expr = fields
            .iter()
            .find_map(|&(n, e)| (n == "tag").then_some(e))
            .ok_or_else(|| Diagnostic::new(codes::TYPE_MISMATCH, "variant literal needs a tag"))?;
        let Expr::String(tag) = tag_expr else {
            bail_unsupported!("variant construction needs a literal tag");
        };
        let variant = TypedHandle::<VariantType>::from_handle(ty, ctx).to_diag(ctx)?;
        let payload = variant
            .deref(ctx)
            .cases()
            .iter()
            .find(|(name, _)| name == tag)
            .map(|(_, payload)| *payload)
            .ok_or_else(|| {
                Diagnostic::new(
                    codes::TYPE_MISMATCH,
                    "literal tag is not a case of this variant",
                )
            })?;
        let mut value = None;
        for &(name, arg) in fields {
            if self.erased_operand(arg) {
                self.discard_function_operand(ctx, arg)?;
            } else if name == "val" {
                let payload = payload.ok_or_else(|| {
                    diag!(
                        codes::TYPE_MISMATCH,
                        "variant val must match its active case"
                    )
                })?;
                let raw = self.expression_with(ctx, arg, Some(payload))?;
                value = Some(self.fit(ctx, raw, payload)?);
            }
        }
        ensure_diag!(
            payload.is_some() == value.is_some(),
            codes::TYPE_MISMATCH,
            "variant val must match its active case"
        );
        Ok(self.emit_value(ctx, |ctx| VariantNewOp::new(ctx, ty, tag, value)))
    }
}
