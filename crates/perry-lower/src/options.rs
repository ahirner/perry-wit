//! Typed literal options retain argument effects and apply defaults after evaluation.
use super::*;

impl FunctionBuilder<'_> {
    pub(super) fn optional_argument(
        &mut self,
        ctx: &mut Context,
        expression: Option<&Expr>,
    ) -> Result<Option<Value>> {
        match expression {
            None | Some(Expr::Undefined) => Ok(None),
            Some(Expr::Call { callee, args, .. }) => self.call(ctx, callee, args),
            Some(expression) => self.expression(ctx, expression).map(Some),
        }
    }

    pub(super) fn fit_optional(
        &mut self,
        ctx: &mut Context,
        value: Option<Value>,
        ty: TypeHandle,
    ) -> Result<Value> {
        let target = perry_mir::types::OptionalType::get(ctx, ty).into();
        match value {
            Some(value) => self.fit(ctx, value, target),
            None => Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, ty))),
        }
    }

    pub(super) fn literal_options(
        &mut self,
        ctx: &mut Context,
        expression: Option<&Expr>,
        names: &[&str],
    ) -> Result<Vec<Option<Value>>> {
        let mut values = vec![None; names.len()];
        let fields: Vec<(String, &Expr)> = match expression {
            None | Some(Expr::Undefined | Expr::Null) => return Ok(values),
            Some(Expr::Object(fields)) => fields
                .iter()
                .map(|(name, value)| (name.clone(), value))
                .collect(),
            Some(Expr::New {
                class_name, args, ..
            }) => self
                .shapes
                .literal_fields(class_name)
                .context("options must be literal data properties")?
                .into_iter()
                .zip(args)
                .collect(),
            _ => bail!("options must be literal data properties"),
        };
        for (name, expression) in fields {
            if let Some(index) = names.iter().position(|&known| known == name) {
                values[index] = self.optional_argument(ctx, Some(expression))?;
            } else {
                self.effect(ctx, expression)?;
            }
        }
        Ok(values)
    }

    pub(super) fn boolean_option(
        &mut self,
        ctx: &mut Context,
        value: Option<Value>,
        default: bool,
    ) -> Result<Value> {
        let Some(value) = value else {
            return Ok(self.boolean(ctx, default));
        };
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let value = self.fit_optional(ctx, Some(value), boolean)?;
        let comparison = self.boolean(ctx, !default);
        let comparison = self.optional(ctx, comparison);
        Ok(self.compare(
            ctx,
            if default {
                CompareKind::Ne
            } else {
                CompareKind::Eq
            },
            value,
            comparison,
        ))
    }
}
