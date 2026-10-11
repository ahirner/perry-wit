//! Closed union construction and tag-based payload selection.
use super::*;
use perry_mir::{
    types::VariantType,
    variants::{VariantNewOp, VariantPayloadOp, VariantTagOp},
};

/// A path through declared fields; reads are checked even if an alias changes the path.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) struct Path(LocalId, Vec<String>);
fn path(expr: &Expr) -> Option<Path> {
    match expr {
        Expr::LocalGet(id) => Some(Path(*id, vec![])),
        Expr::PropertyGet {
            object, property, ..
        } => {
            let mut path = path(object)?;
            path.1.push(property.clone());
            Some(path)
        }
        _ => None,
    }
}

impl FunctionBuilder<'_> {
    pub(super) fn variant_condition(&mut self, condition: &Expr, truth: bool) {
        if let Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } = condition
        {
            self.variant_condition(operand, !truth);
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
    pub(super) fn variant_equality(&mut self, left: &Expr, right: &Expr, equal: bool) {
        let (object, tag) = match (left, right) {
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
            ) if property == "tag" => (object, tag),
            (Expr::TypeOf(object), Expr::String(tag))
            | (Expr::String(tag), Expr::TypeOf(object)) => (object, tag),
            _ => return,
        };
        if let Some(path) = path(object) {
            self.variant_guards.push((path, tag.clone(), equal));
        }
    }

    /// The guards in effect for `path`: each tag and whether the value equals it.
    fn guards<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a str, bool)> + Clone {
        self.variant_guards
            .iter()
            .filter(move |(guarded, _, _)| guarded == path)
            .map(|(_, tag, equal)| (tag.as_str(), *equal))
    }

    pub(super) fn narrow_primitive(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
        value: Value,
    ) -> Result<Value> {
        let ty = optional_value(ctx, value.get_type(ctx)).unwrap_or(value.get_type(ctx));
        if !ty.deref(ctx).is::<perry_mir::types::NumberStringType>() {
            return Ok(value);
        }
        let Some(path) = path(expr) else {
            return Ok(value);
        };
        // The single tag the guards admit, and whether a positive guard names it.
        let (tag, asserted) = {
            let guards = self.guards(&path);
            if guards.clone().next().is_none() {
                return Ok(value);
            }
            let tags: Vec<&'static str> = ["number", "string"]
                .into_iter()
                .filter(|tag| {
                    guards
                        .clone()
                        .all(|(guard, equal)| (*tag == guard) == equal)
                })
                .collect();
            let [tag] = tags.as_slice() else {
                return Ok(value);
            };
            (
                *tag,
                guards.clone().any(|(guard, equal)| equal && guard == *tag),
            )
        };
        // Negative typeof guards alone do not remove null or undefined.
        if !asserted && self.is_optional(ctx, value) {
            return Ok(value);
        }
        let result = if tag == "number" {
            FP64Type::get(ctx).into()
        } else {
            StringType::get(ctx).into()
        };
        let value = self.defined(ctx, value)?;
        Ok(self.emit_value(ctx, |ctx| VariantPayloadOp::new(ctx, value, result)))
    }

    pub(super) fn variant_member(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        value: Value,
        field: &str,
    ) -> Result<Value> {
        if field == "tag" {
            return Ok(self.emit_value(ctx, |ctx| VariantTagOp::new(ctx, value)));
        }
        ensure!(field == "val", "variants have tag and val only");
        let variant = TypedHandle::<VariantType>::from_handle(value.get_type(ctx), ctx)?;
        let mut cases = variant.deref(ctx).cases().to_vec();
        if let Some(path) = path(object) {
            let guards = self.guards(&path);
            cases.retain(|(tag, _)| guards.clone().all(|(guard, equal)| (tag == guard) == equal));
        }
        ensure!(
            !cases.is_empty(),
            "variant payload access has no reachable case"
        );
        let mut payload = None;
        let mut absent = false;
        for (_, ty) in cases {
            match ty {
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
        let result = match payload {
            Some(ty) if absent => perry_mir::types::OptionalType::get(ctx, ty).into(),
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
        args: &[Expr],
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
