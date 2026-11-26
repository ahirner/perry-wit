//! Explicit, checked JSON schema conversion into ordinary native aggregates.
use super::*;
use perry_hir::ir::ObjectPart;
use perry_mir::JsonSchema;

/// Accepted deviation: JSON revivers and indentation are diagnosed at compile time.
pub(super) const JSON_CALLBACKS: &str = "JSON revivers and indentation are not supported (accepted profile): parse into a typed shape and transform it in code";

impl<'s> FunctionBuilder<'s, '_> {
    /// `JSON.stringify(value)`.
    pub(super) fn stringify(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let mut value = self.expression(ctx, expr)?;
        if let Some(src) = self.integer64_source(ctx, value) {
            value = src;
        } else if let Expr::LocalGet(id) = expr
            && let Some(int_ty) = self.locals.get(id).and_then(|local| local.writes.integer)
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
        let callback = self.optional_callback(ctx, replacer)?;
        let keys = if callback.is_some() {
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
        let replacement = if let Some(callback) = callback {
            let key = self.intrinsic(ctx, Intrinsic::JsonReplacerKey, vec![traversal]);
            let json = JsonType::get(ctx).into();
            let json =
                perry_mir::OptionalType::get_with(ctx, json, perry_mir::Absence::Both).into();
            let value = self.inline_callback(
                ctx,
                callback,
                &[key, value],
                Some(crate::lower::inline::InlineResult::Declared(json)),
            )?;
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
        ensure_unsupported!(
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
            _ => bail_unsupported!("JSON equality requires a string, number or boolean operand"),
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
        parts: &'s [ObjectPart],
        expected: Option<TypeHandle>,
        solved: Option<crate::engine::types::Ty>,
    ) -> Result<Value> {
        use crate::engine::types::{Ty, TyKind};
        if parts
            .iter()
            .any(|part| part.defines_key() && hir_compat::static_property(part).is_none())
        {
            bail_unsupported!("computed object spread properties require a static string key");
        }
        let record = |ty| {
            perry_mir::typed::<RecordType>(ctx, present_type(ctx, ty))
                .filter(|r| r.deref(ctx).kind() == RecordKind::Object)
        };
        let expected = expected.and_then(record);
        if solved != Some(Ty::DYNAMIC) {
            let shape = solved
                .filter(|&ty| matches!(self.shapes.types.source.kind(ty), TyKind::Object { .. }));
            if shape.is_some() || expected.is_some() {
                let hint = match expected {
                    Some(record) => Some(record),
                    None => shape
                        .filter(|&ty| !self.shapes.types.source.is_open(ty))
                        .map(|ty| self.shapes.resolved_binding(ty))
                        .transpose()?
                        .and_then(|b| record(b.carrier)),
                };
                return self.record_object_spread(ctx, parts, expected, hint);
            }
        }
        let object = self.intrinsic(ctx, Intrinsic::JsonObject, vec![]);
        let json = object.get_type(ctx);
        for part in parts {
            let raw = self.expression_with(ctx, part.value(), Some(json))?;
            if !part.defines_key() && matches!(kind(ctx, raw), Ok(Kind::Exception)) {
                continue;
            }
            let value = self.fit(ctx, raw, json)?;
            if let Some(key) = hir_compat::static_property(part) {
                let key = self.string(ctx, key);
                self.intrinsic_void(ctx, Intrinsic::JsonSet, vec![object, key, value]);
            } else {
                self.intrinsic_void(ctx, Intrinsic::JsonAssign, vec![object, value]);
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
        self.match_optional(
            ctx,
            value,
            json,
            |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonUndefined, vec![])),
            |b, ctx| Ok(b.intrinsic(ctx, Intrinsic::JsonNull, vec![])),
            |_, _, present| Ok(present),
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
            _ => bail_unsupported!("unsupported Error cause type"),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value]))
    }
}
