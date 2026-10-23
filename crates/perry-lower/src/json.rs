//! Explicit, checked JSON schema conversion into ordinary native aggregates.
use super::*;
use perry_mir::JsonSchema;

/// Accepted deviation: JSON revivers and indentation are diagnosed at compile time.
pub(super) const JSON_CALLBACKS: &str = "JSON revivers and indentation are not supported (accepted profile): parse into a typed shape and transform it in code";

impl<'s> FunctionBuilder<'s> {
    /// `JSON.stringify(value)`.
    pub(super) fn stringify(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let mut value = self.expression(ctx, expr)?;
        if let Some(src) = self.integer64_source(ctx, value) {
            value = src;
        } else if let Expr::LocalGet(id) = expr
            && let Some(int_ty) = self.locals.get(id).and_then(|local| local.integer)
            && int_ty.deref(ctx).width() == 64
        {
            let int_handle: TypeHandle = int_ty.into();
            value = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, int_handle));
        }
        Ok(self.emit_value(ctx, |ctx| JsonStringifyOp::new(ctx, value)))
    }

    pub(super) fn stringify_replaced(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        replacer: &'s Expr,
    ) -> Result<Value> {
        if matches!(replacer, Expr::Null | Expr::Undefined) {
            return self.stringify(ctx, expr);
        }
        let value = self.expression(ctx, expr)?;
        let value = self.emit_value(ctx, |ctx| perry_mir::JsonBoxOp::new(ctx, value));
        let callback = matches!(replacer, Expr::Closure { .. } | Expr::FuncRef(_))
            || matches!(replacer, Expr::LocalGet(id) if self.closures.get(id).is_some());
        let keys = if callback {
            self.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])
        } else {
            let expected = matches!(replacer, Expr::Array(items, _) if items.is_empty())
                .then(|| ArrayType::get(ctx, StringType::get(ctx).into()).into());
            let keys = self.expression_with(ctx, replacer, expected)?;
            self.emit_value(ctx, |ctx| perry_mir::JsonBoxOp::new(ctx, keys))
        };
        let traversal = self.intrinsic(ctx, Intrinsic::JsonReplacerBegin, vec![value, keys]);
        let body = self.new_block(ctx, "json_replacer");
        let done = self.new_block(ctx, "json_written");
        self.jump(ctx, body);
        self.switch_to(body);
        let value = self.intrinsic(ctx, Intrinsic::JsonReplacerValue, vec![traversal]);
        let replacement = if callback {
            let key = self.intrinsic(ctx, Intrinsic::JsonReplacerKey, vec![traversal]);
            let json = JsonType::get(ctx).into();
            let json =
                perry_mir::OptionalType::get_with(ctx, json, perry_mir::Absence::Both).into();
            let value = self.inline_callback(ctx, replacer, &[key, value], Some(json))?;
            self.emit_value(ctx, |ctx| perry_mir::JsonBoxOp::new(ctx, value))
        } else {
            value
        };
        let more = self.intrinsic(
            ctx,
            Intrinsic::JsonReplacerNext,
            vec![traversal, replacement],
        );
        self.branch(ctx, more, body, done);
        self.switch_to(done);
        Ok(self.intrinsic(ctx, Intrinsic::JsonReplacerFinish, vec![traversal]))
    }

    pub(super) fn decode_json(
        &mut self,
        ctx: &mut Context,
        input: Value,
        target: TypeHandle,
    ) -> Result<Value> {
        ensure!(
            JsonSchema::supports_descriptor(ctx, target),
            "unsupported JSON schema"
        );
        Ok(self.emit_value(ctx, |ctx| perry_mir::JsonDecodeOp::new(ctx, input, target)))
    }

    /// Unsupported cursor targets retain the JSON value for later coercion.
    pub(super) fn parse_json_text(
        &mut self,
        ctx: &mut Context,
        text: Value,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        match expected
            .filter(|ty| !ty.deref(ctx).is::<JsonType>() && JsonSchema::supports_cursor(ctx, *ty))
        {
            Some(target) => self.decode_json(ctx, text, target),
            None => Ok(self.intrinsic(ctx, Intrinsic::JsonParse, vec![text])),
        }
    }

    pub(super) fn json_scalar_equal(
        &mut self,
        ctx: &mut Context,
        json: Value,
        scalar: Value,
    ) -> Result<Value> {
        let boolean = IntegerType::get(ctx, 1, Signedness::Signless).into();
        if self.is_optional(ctx, scalar) {
            return self.match_optional(
                ctx,
                scalar,
                boolean,
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonIsUndefined, vec![json])),
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonIsNull, vec![json])),
                |b, ctx, defined| b.json_scalar_equal(ctx, json, defined),
            );
        }
        if kind(ctx, scalar)? == Kind::BigInt {
            let scalar = self.emit_value(ctx, |ctx| perry_mir::JsonBoxOp::new(ctx, scalar));
            return Ok(self.intrinsic(ctx, Intrinsic::JsonStrictEqual, vec![json, scalar]));
        }
        if kind(ctx, scalar)? == Kind::Exception {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonExceptionEqual, vec![json, scalar]));
        }
        if matches!(kind(ctx, scalar)?, Kind::Record | Kind::Array) {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonOriginEqual, vec![json, scalar]));
        }
        let tag = match kind(ctx, scalar)? {
            Kind::String => "string",
            Kind::Number => "number",
            Kind::Boolean => "boolean",
            _ => bail!("JSON equality requires a string, number or boolean operand"),
        };
        let actual = self.intrinsic(ctx, Intrinsic::JsonTypeOf, vec![json]);
        let expected = self.string(ctx, tag);
        let matches = self.compare(ctx, CompareKind::Eq, actual, expected);
        self.choose(
            ctx,
            boolean,
            matches,
            |b, ctx| {
                let decoded = b.fit(ctx, json, scalar.get_type(ctx))?;
                Ok(b.compare(ctx, CompareKind::Eq, decoded, scalar))
            },
            |b, ctx| Ok(b.boolean(ctx, false)),
        )
    }
    pub(super) fn object_spread(
        &mut self,
        ctx: &mut Context,
        parts: &'s [(Option<String>, Expr)],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let expected_record = expected
            .map(|ty| present_type(ctx, ty))
            .and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
            .filter(|rec| rec.deref(ctx).kind() == RecordKind::Object);
        let mut remaining = parts;
        let mut initial = None;
        if let Some((None, first_spread)) = parts.first() {
            let first_src = self.expression(ctx, first_spread)?;
            let is_exc = matches!(kind(ctx, first_src), Ok(Kind::Exception));
            let first_json = if is_exc {
                None
            } else {
                self.json_operand(ctx, first_src)?
            };
            if first_json.is_none()
                && (expected_record.is_some()
                    || (!is_exc
                        && parts[1..]
                            .iter()
                            .all(|(k, v)| k.is_some() || self.peek_record_type(ctx, v).is_some())))
            {
                return self.record_object_spread(ctx, parts, Some(first_src), expected_record);
            }
            if !is_exc {
                initial = Some(first_json.context(
                    "object spread is supported over typed records or JSON values with static keys",
                )?);
            }
            remaining = &parts[1..];
        } else if (expected_record.is_some()
            && !parts.iter().any(|(k, v)| {
                k.is_none()
                    && matches!(v, Expr::LocalGet(id) if self.locals.get(id).and_then(|l| self.slot_type(ctx, l.slot).ok()).is_some_and(|ty| present_type(ctx, ty).deref(ctx).is::<JsonType>()))
            }))
            || (parts.iter().any(|(k, _)| k.is_none())
                && parts
                    .iter()
                    .all(|(k, v)| k.is_some() || self.peek_record_type(ctx, v).is_some()))
        {
            return self.record_object_spread(ctx, parts, None, expected_record);
        }
        let object = self.intrinsic(ctx, Intrinsic::JsonObject, vec![]);
        if let Some(source) = initial {
            self.intrinsic_void(ctx, Intrinsic::JsonAssign, vec![object, source]);
        }
        for (key, value) in remaining {
            if let Some(key) = key {
                let key = self.string(ctx, key);
                let value = self.json_value(ctx, value)?;
                self.intrinsic_void(ctx, Intrinsic::JsonSet, vec![object, key, value]);
            } else {
                let source = self.expression(ctx, value)?;
                if matches!(kind(ctx, source), Ok(Kind::Exception)) {
                    continue;
                }
                let source = self.json_operand(ctx, source)?.context(
                    "object spread is supported over typed records or JSON values with static keys",
                )?;
                self.intrinsic_void(ctx, Intrinsic::JsonAssign, vec![object, source]);
            }
        }
        Ok(object)
    }

    /// A JSON value, or `Json | undefined` (an array element) as one: the JSON `undefined` is
    /// the null address. `None` for other types.
    pub(super) fn json_operand(
        &mut self,
        ctx: &mut Context,
        value: Value,
    ) -> Result<Option<Value>> {
        let ty = value.get_type(ctx);
        if ty.deref(ctx).is::<JsonType>() {
            return Ok(Some(value));
        }
        if !optional_value(ctx, ty).is_some_and(|inner| inner.deref(ctx).is::<JsonType>()) {
            return Ok(None);
        }
        let json = JsonType::get(ctx).into();
        let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
        self.choose(
            ctx,
            json,
            undefined,
            |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])),
            |b, ctx| b.defined(ctx, value),
        )
        .map(Some)
    }

    /// Retain supported Error causes, preserving reference identity alongside their readable fields.
    pub(super) fn error_cause(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if let Some(json) = self.json_operand(ctx, value)? {
            return Ok(json);
        }
        if self.is_optional(ctx, value) {
            let json = JsonType::get(ctx).into();
            return self.match_optional(
                ctx,
                value,
                json,
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])),
                |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonNull, vec![])),
                |b, ctx, defined| b.error_cause(ctx, defined),
            );
        }
        let intrinsic = match kind(ctx, value)? {
            Kind::Number => Intrinsic::JsonFromNumber,
            Kind::String => Intrinsic::JsonFromString,
            Kind::Boolean => Intrinsic::JsonFromBool,
            Kind::Exception => Intrinsic::JsonFromException,
            Kind::Record | Kind::Array => {
                return Ok(self.emit_value(ctx, |ctx| perry_mir::NativeJsonOp::new(ctx, value)));
            }
            _ => bail!("unsupported Error cause type"),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value]))
    }

    /// A member value of a JSON object literal: a JSON value, or a number, string, boolean
    /// or `null` converted to one.
    fn json_value(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        if matches!(expr, Expr::Null) {
            return Ok(self.intrinsic(ctx, Intrinsic::JsonNull, vec![]));
        }
        let value = self.expression(ctx, expr)?;
        if let Some(json) = self.json_operand(ctx, value)? {
            return Ok(json);
        }
        let intrinsic = match kind(ctx, value)? {
            Kind::Number => Intrinsic::JsonFromNumber,
            Kind::String => Intrinsic::JsonFromString,
            Kind::Boolean => Intrinsic::JsonFromBool,
            _ => bail!(
                "members of a spread object literal are JSON values, numbers, strings, \
                 booleans or null so far"
            ),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value]))
    }
}
