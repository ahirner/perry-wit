//! Closed union construction and tag-based payload selection.
use super::*;
use perry_mir::{VariantNewOp, VariantPayloadOp, VariantTagOp, VariantType};

/// Access path through declared fields whose reads re-verify the discriminant tag.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) struct Path<'s>(LocalId, Vec<&'s str>);
pub(super) fn path(expr: &Expr) -> Option<Path<'_>> {
    match expr {
        Expr::LocalGet(id) => Some(Path(*id, vec![])),
        Expr::PropertyGet {
            object, property, ..
        } => {
            let mut path = path(object)?;
            path.1.push(property.as_str());
            Some(path)
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Discriminant {
    TypeOf,
    Tag,
}

pub(super) struct Guard<'s> {
    path: Path<'s>,
    tag: &'s str,
    equal: bool,
    discriminant: Discriminant,
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn variant_condition(&mut self, condition: &'s Expr, truth: bool) {
        if let Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } = condition
        {
            self.variant_condition(operand, !truth);
            return;
        }
        if let Expr::Logical { op, left, right } = condition {
            match (op, truth) {
                (LogicalOp::And, true) => {
                    self.variant_condition(left, true);
                    self.variant_condition(right, true);
                }
                (LogicalOp::Or, false) => {
                    self.variant_condition(left, false);
                    self.variant_condition(right, false);
                }
                _ => {}
            }
            return;
        }
        let Expr::Compare { op, left, right } = condition else {
            return;
        };
        match op {
            CompareOp::Eq => self.variant_equality(left, right, truth),
            CompareOp::Ne => self.variant_equality(left, right, !truth),
            _ => {}
        }
    }

    /// Record that `left === right` is `equal` when one side is a tag or `typeof` of a
    /// path and the other a string.
    pub(super) fn variant_equality(&mut self, left: &'s Expr, right: &'s Expr, equal: bool) {
        let (object, tag, discriminant) = match (left, right) {
            (
                Expr::PropertyGet {
                    object, property, ..
                },
                Expr::String(tag),
            )
            | (
                Expr::String(tag),
                Expr::PropertyGet {
                    object, property, ..
                },
            ) if property == "tag" => (object, tag, Discriminant::Tag),
            (Expr::TypeOf(object), Expr::String(tag))
            | (Expr::String(tag), Expr::TypeOf(object)) => (object, tag, Discriminant::TypeOf),
            _ => return,
        };
        if let Some(path) = path(object) {
            self.variant_guards.push(Guard {
                path,
                tag: tag.as_str(),
                equal,
                discriminant,
            });
        }
    }

    /// The guards in effect for `path`: each tag and whether the value equals it.
    fn guards<'a>(
        &'a self,
        path: &'a Path<'s>,
        discriminant: Discriminant,
    ) -> impl Iterator<Item = (&'s str, bool)> + 'a {
        self.variant_guards
            .iter()
            .filter(move |guard| &guard.path == path && guard.discriminant == discriminant)
            .map(|guard| (guard.tag, guard.equal))
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
            self.guards(&path, Discriminant::TypeOf)
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
            .guards(&path, Discriminant::TypeOf)
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
                    .guards(path, Discriminant::Tag)
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
