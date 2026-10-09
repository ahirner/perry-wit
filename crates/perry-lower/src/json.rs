//! Explicit JSON variants and checked conversion back to statically typed scalar values.
use super::*;
use perry_mir::types::OptionalType;

pub(super) fn is_json(ctx: &Context, value: Value) -> bool {
    let ty = optional_value(ctx, value.get_type(ctx)).unwrap_or(value.get_type(ctx));
    ty.deref(ctx).is::<JsonType>()
}

impl FunctionBuilder<'_> {
    pub(super) fn type_of(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if is_json(ctx, value) {
            let value = self.optional(ctx, value);
            return Ok(self
                .intrinsic(ctx, Intrinsic::JsonTypeof, vec![value])
                .unwrap());
        }
        let optional = optional_value(ctx, value.get_type(ctx));
        let ty = optional.unwrap_or(value.get_type(ctx));
        let tag = {
            let ty = ty.deref(ctx);
            if ty.is::<FP64Type>() {
                "number"
            } else if ty.is::<StringType>() {
                "string"
            } else if ty
                .downcast_ref::<IntegerType>()
                .is_some_and(|ty| ty.width() == 1)
            {
                "boolean"
            } else if ty.is::<RecordType>()
                || ty.is::<ArrayType>()
                || ty.is::<TupleType>()
                || ty.is::<DictionaryType>()
                || ty.is::<TextEncoderType>()
                || ty.is::<TextDecoderType>()
                || ty.is::<BytesType>()
                || ty.is::<BufferType>()
                || ty.is::<PromiseType>()
            {
                "object"
            } else {
                bail!("typeof is not supported for this value type yet");
            }
        };
        let present = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, tag));
        if optional.is_none() {
            return Ok(present);
        }
        let absent = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
        let missing = self.new_block(ctx, "typeof_missing");
        let defined = self.new_block(ctx, "typeof_defined");
        let join = self.new_block(ctx, "typeof_join");
        let result = self.entry_slot(ctx, present.get_type(ctx));
        self.branch(ctx, absent, missing, defined);
        self.switch_to(missing);
        let undefined = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, "undefined"));
        self.store(ctx, result, undefined);
        self.jump(ctx, join);
        self.switch_to(defined);
        self.store(ctx, result, present);
        self.jump(ctx, join);
        self.switch_to(join);
        self.load(ctx, result)
    }

    pub(super) fn json_key(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let key = self.expression(ctx, expr)?;
        let key = self.arithmetic(ctx, key);
        ensure!(
            matches!(kind(ctx, key)?, Kind::Number | Kind::String),
            "JSON computed keys must be strings or numbers"
        );
        Ok(key)
    }
    pub(super) fn stringify_json(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
        controls: Option<(&Expr, &Expr)>,
    ) -> Result<Value> {
        if let Some((replacer, space)) = controls {
            ensure!(
                matches!(replacer, Expr::Null | Expr::Undefined)
                    && matches!(space, Expr::Null | Expr::Undefined),
                "JSON.stringify replacers and formatting are outside the supported subset"
            );
        }
        let value = self.json_value(ctx, expr)?;
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
                    let value = self.json_value(ctx, item)?;
                    values.push(self.optional(ctx, value));
                }
                let json = JsonType::get(ctx).into();
                let json = OptionalType::get(ctx, json).into();
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
            Expr::Undefined => {
                let json = JsonType::get(ctx).into();
                Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, json)))
            }
            _ => {
                let value = self.expression(ctx, expr)?;
                self.json_input(ctx, value)
            }
        }
    }

    pub(super) fn json_input(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if is_json(ctx, value) {
            return Ok(value);
        }
        ensure!(
            perry_mir::json::compatible(ctx, value.get_type(ctx)),
            "value is not JSON-compatible"
        );
        Ok(self.emit_value(ctx, |ctx| perry_mir::json::PackOp::new(ctx, value)))
    }

    pub(super) fn json_assignment(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
    ) -> Result<(Value, Value)> {
        let value = self.object_operand(ctx, expr)?;
        let json = self.json_input(ctx, value)?;
        Ok((value, self.optional(ctx, json)))
    }

    pub(super) fn object_operand(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Array(_) | Expr::Object(_) => self.json_value(ctx, expr),
            Expr::New { class_name, .. } if self.shapes.literal_fields(class_name).is_some() => {
                self.json_value(ctx, expr)
            }
            _ => self.expression(ctx, expr),
        }
    }

    pub(super) fn enumeration_operand(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Array(items) => {
                let values = items
                    .iter()
                    .map(|item| self.enumeration_operand(ctx, item))
                    .collect::<Result<Vec<_>>>()?;
                let Some(element) = perry_mir::enumeration::common_type(
                    ctx,
                    values.iter().map(|value| value.get_type(ctx)),
                ) else {
                    return Ok(
                        self.emit_value(ctx, |ctx| perry_mir::tuples::NewOp::new(ctx, values))
                    );
                };
                let values = values
                    .into_iter()
                    .map(|value| self.fit(ctx, value, element))
                    .collect::<Result<Vec<_>>>()?;
                Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, values)))
            }
            Expr::Object(fields) => {
                let fields = fields
                    .iter()
                    .map(|(name, expr)| Ok((name.clone(), self.enumeration_operand(ctx, expr)?)))
                    .collect::<Result<_>>()?;
                self.enumeration_fields(ctx, fields)
            }
            Expr::New {
                class_name, args, ..
            } if self.shapes.literal_fields(class_name).is_some() => {
                let names = self.shapes.literal_fields(class_name).unwrap();
                let fields = names
                    .iter()
                    .zip(args)
                    .map(|(name, expr)| Ok((name.clone(), self.enumeration_operand(ctx, expr)?)))
                    .collect::<Result<_>>()?;
                self.enumeration_fields(ctx, fields)
            }
            _ => self.expression(ctx, expr),
        }
    }

    fn enumeration_fields(
        &mut self,
        ctx: &mut Context,
        fields: Vec<(String, Value)>,
    ) -> Result<Value> {
        let mut names = std::collections::HashSet::new();
        if fields
            .iter()
            .all(|(name, _)| identifier(name).is_ok() && names.insert(name))
        {
            return self.record_from_values(ctx, fields, None);
        }
        ensure!(
            !fields.iter().any(|(name, _)| name == "__proto__"),
            "prototype setters are outside the closed-shape subset"
        );
        let element = perry_mir::enumeration::common_type(ctx,fields.iter().map(|(_, value)| value.get_type(ctx)))
            .context("enumerated literal fields with arbitrary keys must share a native type or be JSON-compatible")?;
        let fields = fields
            .into_iter()
            .map(|(name, value)| Ok((name, self.fit(ctx, value, element)?)))
            .collect::<Result<_>>()?;
        let ty = DictionaryType::get(ctx, element).into();
        Ok(self.emit_value(ctx, |ctx| {
            perry_mir::dictionaries::NewOp::new(ctx, ty, fields)
        }))
    }

    pub(super) fn property_key(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let key = self.object_operand(ctx, expr)?;
        if key.get_type(ctx).deref(ctx).is::<StringType>() {
            return Ok(key);
        }
        let key = self.json_input(ctx, key)?;
        let key = self.optional(ctx, key);
        Ok(self.intrinsic(ctx, Intrinsic::JsonKey, vec![key]).unwrap())
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
            let value = self.json_value(ctx, value)?;
            values.push(self.optional(ctx, value));
        }
        let string = StringType::get(ctx).into();
        let json = JsonType::get(ctx).into();
        let json = OptionalType::get(ctx, json).into();
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
            Kind::Record | Kind::Array | Kind::Tuple | Kind::Dictionary => {
                ensure!(
                    perry_mir::json::compatible(ctx, value.get_type(ctx)),
                    "aggregate contains values outside the JSON subset: {}",
                    value.get_type(ctx).disp(ctx)
                );
                return Ok(self.emit_value(ctx, |ctx| perry_mir::json::PackOp::new(ctx, value)));
            }
            _ => bail!("value is not JSON-compatible"),
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
        } else {
            ensure!(
                perry_mir::json::compatible(ctx, target),
                "JSON target contains unsupported types"
            );
            return Ok(self.emit_value(ctx, |ctx| {
                perry_mir::json::UnpackOp::new(ctx, value, target)
            }));
        };
        let value = self.optional(ctx, value);
        Ok(self.intrinsic(ctx, intrinsic, vec![value]).unwrap())
    }
}
