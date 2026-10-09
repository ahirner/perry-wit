//! Explicit JSON variants and checked conversion back to statically typed scalar values.
use super::*;

pub(super) fn is_json(ctx: &Context, value: Value) -> bool {
    let ty = optional_value(ctx, value.get_type(ctx)).unwrap_or(value.get_type(ctx));
    ty.deref(ctx).is::<JsonType>()
}

impl FunctionBuilder<'_> {
    pub(super) fn json_key(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let key = self.expression(ctx, expr)?;
        let key = self.arithmetic(ctx, key);
        ensure!(
            matches!(kind(ctx, key)?, Kind::Number | Kind::String),
            "JSON computed keys must be strings or numbers"
        );
        Ok(key)
    }
    pub(super) fn stringify_json(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let value = match expr {
            Expr::Undefined => {
                let json = JsonType::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, json))
            }
            Expr::Array(_) | Expr::Object(_) | Expr::New { .. } => self.json_value(ctx, expr)?,
            _ => {
                let value = self.expression(ctx, expr)?;
                if is_json(ctx, value) {
                    value
                } else {
                    self.pack_json(ctx, value)?
                }
            }
        };
        let value = self.optional(ctx, value);
        Ok(self
            .intrinsic(ctx, Intrinsic::JsonStringify, vec![value])
            .unwrap())
    }
    pub(super) fn json_value(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.json_value(ctx, item)?);
                }
                let json = JsonType::get(ctx).into();
                let array = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, json, values));
                Ok(self
                    .intrinsic(ctx, Intrinsic::JsonArray, vec![array])
                    .unwrap())
            }
            Expr::Object(fields) => self.json_object(
                ctx,
                fields.iter().map(|(name, value)| (name.as_str(), value)),
            ),
            Expr::New {
                class_name, args, ..
            } if self.shapes.literal_fields(class_name).is_some() => {
                let names = self.shapes.literal_fields(class_name).unwrap();
                self.json_object(ctx, names.iter().map(String::as_str).zip(args))
            }
            _ => {
                let value = self.expression(ctx, expr)?;
                self.pack_json(ctx, value)
            }
        }
    }

    fn json_object<'e>(
        &mut self,
        ctx: &mut Context,
        fields: impl Iterator<Item = (&'e str, &'e Expr)>,
    ) -> Result<Value> {
        let mut keys = Vec::with_capacity(fields.size_hint().0);
        let mut values = Vec::with_capacity(fields.size_hint().0);
        for (name, value) in fields {
            ensure!(
                name != "__proto__",
                "prototype setters are outside the closed-shape subset"
            );
            keys.push(self.emit_value(ctx, |ctx| StringConstOp::new(ctx, name)));
            values.push(self.json_value(ctx, value)?);
        }
        let string = StringType::get(ctx).into();
        let json = JsonType::get(ctx).into();
        let keys = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, string, keys));
        let values = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, json, values));
        Ok(self
            .intrinsic(ctx, Intrinsic::JsonObject, vec![keys, values])
            .unwrap())
    }

    pub(super) fn pack_json(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        let value = self.defined(ctx, value)?;
        let intrinsic = match kind(ctx, value)? {
            Kind::Json => return Ok(value),
            Kind::Number => Intrinsic::JsonNumber,
            Kind::Boolean => Intrinsic::JsonBoolean,
            Kind::String => Intrinsic::JsonString,
            Kind::Array if self.array_element(ctx, value)?.deref(ctx).is::<JsonType>() => {
                Intrinsic::JsonArray
            }
            _ => bail!("JSON conversion of stored aggregates is not implemented yet"),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value]).unwrap())
    }

    pub(super) fn narrow_json(
        &mut self,
        ctx: &mut Context,
        value: Value,
        target: TypeHandle,
    ) -> Result<Value> {
        let intrinsic = if target.deref(ctx).is::<FP64Type>() {
            Intrinsic::JsonAsNumber
        } else if target.deref(ctx).is::<StringType>() {
            Intrinsic::JsonAsString
        } else if target
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() == 1)
        {
            Intrinsic::JsonAsBoolean
        } else if target
            .deref(ctx)
            .downcast_ref::<ArrayType>()
            .is_some_and(|array| array.element().deref(ctx).is::<JsonType>())
        {
            Intrinsic::JsonAsArray
        } else {
            bail!(
                "JSON narrowing to aggregate types needs typed views, which are not implemented yet"
            );
        };
        let value = self.optional(ctx, value);
        Ok(self.intrinsic(ctx, intrinsic, vec![value]).unwrap())
    }
}
