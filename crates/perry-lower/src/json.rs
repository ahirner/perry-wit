//! Explicit JSON variants and schema-based conversion to and from native values.
use super::*;

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
        if optional_value(ctx, value.get_type(ctx)).is_some() {
            let absent = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            let missing = self.new_block(ctx, "typeof_missing");
            let defined = self.new_block(ctx, "typeof_defined");
            let join = self.new_block(ctx, "typeof_join");
            let result = self.entry_slot(ctx, StringType::get(ctx).into());
            self.branch(ctx, absent, missing, defined);
            self.switch_to(missing);
            let undefined = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, "undefined"));
            self.store(ctx, result, undefined);
            self.jump(ctx, join);
            self.switch_to(defined);
            let value = self.defined(ctx, value)?;
            let tag = self.type_of(ctx, value)?;
            self.store(ctx, result, tag);
            self.jump(ctx, join);
            self.switch_to(join);
            return self.load(ctx, result);
        }
        if value.get_type(ctx).deref(ctx).is::<TextOrBytesType>() {
            let text = self
                .intrinsic(ctx, Intrinsic::TextBytesIsText, vec![value])
                .unwrap();
            let result = self.entry_slot(ctx, StringType::get(ctx).into());
            let then = self.new_block(ctx, "typeof_text");
            let other = self.new_block(ctx, "typeof_bytes");
            let done = self.new_block(ctx, "typeof_done");
            self.branch(ctx, text, then, other);
            self.switch_to(then);
            let tag = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, "string"));
            self.store(ctx, result, tag);
            self.jump(ctx, done);
            self.switch_to(other);
            let tag = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, "object"));
            self.store(ctx, result, tag);
            self.jump(ctx, done);
            self.switch_to(done);
            return self.load(ctx, result);
        }
        let ty = value.get_type(ctx);
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
                || ty.is::<FsStatsType>()
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
        Ok(self.emit_value(ctx, |ctx| StringConstOp::new(ctx, tag)))
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
            Expr::Undefined => {
                let json = JsonType::get(ctx).into();
                Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, json)))
            }
            Expr::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    let value = self.json_value(ctx, item)?;
                    values.push(self.optional(ctx, value));
                }
                let json =
                    perry_mir::types::OptionalType::get(ctx, JsonType::get(ctx).into()).into();
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
            let value = self.json_value(ctx, value)?;
            values.push(self.optional(ctx, value));
        }
        let string = StringType::get(ctx).into();
        let json = perry_mir::types::OptionalType::get(ctx, JsonType::get(ctx).into()).into();
        let keys = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, string, keys));
        let values = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, json, values));
        Ok(self
            .intrinsic(ctx, Intrinsic::JsonObject, vec![keys, values])
            .unwrap())
    }

    pub(super) fn pack_json(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        self.encode_json(ctx, value, 0)
    }

    /// Schema conversion builds a JSON snapshot; native aggregates never become dynamic views.
    fn encode_json(&mut self, ctx: &mut Context, value: Value, depth: usize) -> Result<Value> {
        ensure!(
            depth < 32,
            "recursive or deeply nested native JSON schemas are unsupported"
        );
        if is_json(ctx, value) {
            return Ok(value);
        }
        if optional_value(ctx, value.get_type(ctx)).is_some() {
            let json = JsonType::get(ctx).into();
            let absent = self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, json));
            let slot = self.entry_slot(ctx, absent.get_type(ctx));
            self.store(ctx, slot, absent);
            let missing = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            let present = self.new_block(ctx, "json_present");
            let done = self.new_block(ctx, "json_optional");
            self.branch(ctx, missing, done, present);
            self.switch_to(present);
            let defined = self.defined(ctx, value)?;
            let encoded = self.encode_json(ctx, defined, depth + 1)?;
            let encoded = self.optional(ctx, encoded);
            self.store(ctx, slot, encoded);
            self.jump(ctx, done);
            self.switch_to(done);
            return self.load(ctx, slot);
        }
        let value = self.defined(ctx, value)?;
        if let Ok(record) = TypedHandle::<RecordType>::from_handle(value.get_type(ctx), ctx) {
            ensure!(
                record.deref(ctx).name().as_ref() != "builtin_Date",
                "Date JSON serialization is not supported yet"
            );
            let fields = record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("opaque JSON schema"))?
                .to_vec();
            let mut keys = Vec::new();
            let mut values = Vec::new();
            for (name, _) in fields {
                keys.push(self.emit_value(ctx, |ctx| StringConstOp::new(ctx, name.as_ref())));
                let field = FieldGetOp::new(ctx, value, name)?;
                let field = self.append_value(ctx, &field);
                let encoded = self.encode_json(ctx, field, depth + 1)?;
                values.push(self.optional(ctx, encoded));
            }
            let string = StringType::get(ctx).into();
            let json = perry_mir::types::OptionalType::get(ctx, JsonType::get(ctx).into()).into();
            let keys = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, string, keys));
            let values = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, json, values));
            return Ok(self
                .intrinsic(ctx, Intrinsic::JsonObject, vec![keys, values])
                .unwrap());
        }
        if value.get_type(ctx).deref(ctx).is::<ArrayType>() {
            let json = perry_mir::types::OptionalType::get(ctx, JsonType::get(ctx).into()).into();
            let encoded = self.convert_json_array(ctx, value, json, depth + 1, true)?;
            return Ok(self
                .intrinsic(ctx, Intrinsic::JsonArray, vec![encoded])
                .unwrap());
        }
        let intrinsic = match kind(ctx, value).context("value is not JSON-compatible")? {
            Kind::Json => return Ok(value),
            Kind::Number => Intrinsic::JsonNumber,
            Kind::Boolean => Intrinsic::JsonBoolean,
            Kind::String => Intrinsic::JsonString,
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
        self.decode_json(ctx, value, target, 0)
    }

    fn decode_json(
        &mut self,
        ctx: &mut Context,
        value: Value,
        target: TypeHandle,
        depth: usize,
    ) -> Result<Value> {
        ensure!(
            depth < 32,
            "recursive or deeply nested native JSON schemas are unsupported"
        );
        if target.deref(ctx).is::<JsonType>() {
            return self.defined(ctx, value);
        }
        if let Ok(record) = TypedHandle::<RecordType>::from_handle(target, ctx) {
            ensure!(
                record.deref(ctx).name().as_ref() != "builtin_Date",
                "Date JSON conversion is not supported"
            );
            let fields = record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("opaque JSON schema"))?
                .to_vec();
            let mut values = Vec::new();
            let value = self.optional(ctx, value);
            let value = self
                .intrinsic(ctx, Intrinsic::JsonAsObject, vec![value])
                .unwrap();
            for (name, ty) in fields {
                let key = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, name.as_ref()));
                let field = self
                    .intrinsic(ctx, Intrinsic::JsonGet, vec![value, key])
                    .unwrap();
                values.push(self.decode_json(ctx, field, ty, depth + 1)?);
            }
            return Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values)));
        }
        if let Ok(array) = TypedHandle::<ArrayType>::from_handle(target, ctx) {
            let element = array.deref(ctx).element();
            let value = self.optional(ctx, value);
            let source = self
                .intrinsic(ctx, Intrinsic::JsonAsArray, vec![value])
                .unwrap();
            if element.deref(ctx).is::<JsonType>() {
                return Ok(source);
            }
            return self.convert_json_array(ctx, source, element, depth + 1, false);
        }
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
            bail!("JSON schema requires scalars, closed records or dense arrays");
        };
        let value = self.optional(ctx, value);
        Ok(self.intrinsic(ctx, intrinsic, vec![value]).unwrap())
    }

    fn convert_json_array(
        &mut self,
        ctx: &mut Context,
        source: Value,
        element: TypeHandle,
        depth: usize,
        encode: bool,
    ) -> Result<Value> {
        let output = self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, vec![]));
        let number = FP64Type::get(ctx).into();
        let index = self.entry_slot(ctx, number);
        let zero = self.number(ctx, 0.0);
        self.store(ctx, index, zero);
        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, source));
        let length = self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
        });
        let test = self.new_block(ctx, "json_array_test");
        let body = self.new_block(ctx, "json_array_body");
        let done = self.new_block(ctx, "json_array_done");
        self.jump(ctx, test);
        self.switch_to(test);
        let i = self.load(ctx, index)?;
        let more = self.compare(ctx, CompareKind::Lt, i, length);
        self.branch(ctx, more, body, done);
        self.switch_to(body);
        let get = ArrayGetOp::new(ctx, source, i)?;
        let value = self.append_value(ctx, &get);
        let value = if encode {
            let encoded = self.encode_json(ctx, value, depth)?;
            self.optional(ctx, encoded)
        } else {
            self.decode_json(ctx, value, element, depth)?
        };
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, output, value));
        let one = self.number(ctx, 1.0);
        let next = self.binary(ctx, BinaryKind::Add, i, one);
        self.store(ctx, index, next);
        self.jump(ctx, test);
        self.switch_to(done);
        Ok(output)
    }
}
