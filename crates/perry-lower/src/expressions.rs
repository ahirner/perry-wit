//! Expressions: the dispatch over HIR expressions, and locals, literals and member reads.
use super::*;
use crate::array_methods::{Direction, FindResult, Mutation, Quantifier, SearchResult, Transform};
use crate::builtins::{BytesCreation, JSON_CALLBACKS};

mod members;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn expression(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        self.expression_with(ctx, expr, None)
    }

    /// Lower `expr`; `expected` types literals whose own type is not determined by their
    /// operands (empty arrays, object literals widened to a declared shape).
    pub(super) fn expression_with(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        if let Some(record) = self.iteration.expression(expr) {
            return match record {
                hir_compat::RecordExpr::Value(value) => self.expression_with(ctx, value, expected),
                hir_compat::RecordExpr::RequireObject(value) => {
                    let v = self.expression_with(ctx, value, expected)?;
                    self.defined(ctx, v)
                }
                hir_compat::RecordExpr::Length(source) => {
                    let source = self.expression(ctx, source)?;
                    if let Ok(record) =
                        TypedHandle::<RecordType>::from_handle(source.get_type(ctx), ctx)
                        && record.deref(ctx).is_tuple()
                    {
                        let length = record.deref(ctx).fields()?.len();
                        Ok(self.number(ctx, length as f64))
                    } else {
                        Ok(self.array_length(ctx, source))
                    }
                }
                hir_compat::RecordExpr::Close => self.absent_literal(ctx, false, expected),
            };
        }
        if let Expr::Sequence(parts) = expr {
            let (last, effects) = parts.split_last().context("empty sequence expression")?;
            if let Some(span) = self.shapes.metadata.locations.operator_span(parts) {
                return self
                    .expression_with(ctx, last, expected)
                    .map_err(|error| self.shapes.metadata.locations.annotate(error, span));
            }
            for effect in effects {
                self.effect(ctx, effect)?;
            }
            return self.expression_with(ctx, last, expected);
        }
        if let Some(value) = self.date_expression(ctx, expr)? {
            return Ok(value);
        }
        if let Some(value) = self.url_expression(ctx, expr)? {
            return Ok(value);
        }
        if let Some(value) = self.collection_expression(ctx, expr, expected)? {
            return self.narrow_primitive(ctx, expr, value);
        }
        let value = match expr {
            Expr::RegExp { pattern, flags } => {
                self.emit_value(ctx, |ctx| perry_mir::RegexConstOp::new(ctx, pattern, flags))
            }
            Expr::RegExpDynamic { .. } => {
                bail!("RegExp construction is unsupported; use a bounded regex literal")
            }
            Expr::ObjectSpread { parts } => self.object_spread(ctx, parts, expected)?,
            Expr::ObjectRest {
                object,
                exclude_keys,
            } => self.object_rest(ctx, object, exclude_keys, expected)?,
            Expr::BigInt(text) => self.bigint_literal(ctx, text)?,
            Expr::BigIntCoerce(expr) => self.bigint_coerce(ctx, expr)?,
            Expr::NumberCoerce(expr) => self.number_coerce(ctx, expr)?,
            Expr::StringCoerce(expr) | Expr::TemplateStringCoerce(expr) => {
                let value = self.expression(ctx, expr)?;
                if let Some((class_name, type_args)) =
                    self.shapes.class_of_type(ctx, value.get_type(ctx))
                    && let Some((owner_class, func)) =
                        self.shapes.class_method(class_name, "toString")
                {
                    let recv = self.defined(ctx, value)?;
                    self.class_method_call(ctx, recv, owner_class, &type_args, func, &[])?
                        .context("toString() produced no value")?
                } else {
                    self.display(ctx, value)?
                }
            }
            Expr::Number(value) => self.number(ctx, *value),
            Expr::Integer(value) => self.number(ctx, *value as f64),
            Expr::Bool(value) => self.boolean(ctx, *value),
            Expr::String(text) => self.string(ctx, text),
            Expr::LocalGet(id) => self
                .load_local(ctx, *id)
                .map_err(|error| self.shapes.annotate_local(error, *id))?,
            Expr::Null | Expr::Undefined => {
                self.absent_literal(ctx, matches!(expr, Expr::Null), expected)?
            }
            Expr::LocalSet(id, value) => self
                .local_set(ctx, *id, value)
                .map_err(|error| self.shapes.annotate_local(error, *id))?,
            Expr::Update { id, op, prefix } => {
                self.iteration.advance(*id);
                self.update(ctx, *id, op, *prefix)
                    .map_err(|error| self.shapes.annotate_local(error, *id))?
            }
            Expr::PropertyUpdate {
                object,
                property,
                op,
                prefix,
                ..
            } => self.property_update(ctx, object, property, op, *prefix)?,
            Expr::Binary { op, left, right } => self.binary_expression(ctx, op, left, right)?,
            Expr::Compare { op, left, right } => self.comparison(ctx, op, left, right)?,
            Expr::Logical { op, left, right } => self.logical(ctx, op, left, right, expected)?,
            Expr::ScopedTemp { id, value, body } => {
                let temp = self.expression(ctx, value)?;
                let slot = self.entry_slot(ctx, temp.get_type(ctx));
                self.store(ctx, slot, temp);
                let prev = self.locals.insert(*id, LocalBinding::plain(slot));
                let out = self.expression_with(ctx, body, expected);
                if let Some(prev) = prev {
                    self.locals.insert(*id, prev);
                } else {
                    self.locals.remove(id);
                }
                out?
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => self.conditional(ctx, condition, then_expr, else_expr, expected)?,
            Expr::Unary { op, operand } => self.unary(ctx, op, operand)?,
            Expr::StringSplit(text, separator) => {
                let text = self.string_operand(ctx, text)?;
                let separator = self.string_operand(ctx, separator)?;
                self.intrinsic(ctx, Intrinsic::StringSplit, vec![text, separator])
            }
            Expr::CallSpread { callee, args, .. } => self.array_push_spread(ctx, callee, args)?,
            Expr::Call {
                callee,
                args,
                byte_offset,
                ..
            } => self
                .call(ctx, callee, args, *byte_offset)
                .and_then(|value| value.context("a void call has no value"))
                .map_err(|error| self.shapes.annotate_call(error, *byte_offset))?,
            Expr::MathFloor(value) => self.math_unary(ctx, Intrinsic::MathFloor, value)?,
            Expr::MathCeil(value) => self.math_unary(ctx, Intrinsic::MathCeil, value)?,
            Expr::MathTrunc(value) => self.math_unary(ctx, Intrinsic::MathTrunc, value)?,
            Expr::MathAbs(value) => self.math_unary(ctx, Intrinsic::MathAbs, value)?,
            Expr::MathRound(value) => self.math_unary(ctx, Intrinsic::MathRound, value)?,
            Expr::MathSqrt(value) => self.math_unary(ctx, Intrinsic::MathSqrt, value)?,
            Expr::MathMin(values) => {
                self.math_extremum(ctx, Intrinsic::MathMin, f64::INFINITY, values)?
            }
            Expr::MathMax(values) => {
                self.math_extremum(ctx, Intrinsic::MathMax, f64::NEG_INFINITY, values)?
            }
            Expr::MathMinSpread(array) => {
                self.math_extremum_spread(ctx, Intrinsic::MathMin, f64::INFINITY, array)?
            }
            Expr::MathMaxSpread(array) => {
                self.math_extremum_spread(ctx, Intrinsic::MathMax, f64::NEG_INFINITY, array)?
            }
            Expr::StructuredClone { value, .. } => {
                let val = self.expression_with(ctx, value, expected)?;
                self.clone_structured_value(ctx, val)?
            }
            Expr::ParseInt { string, radix } => {
                let value = self.expression(ctx, string)?;
                let text = self.display(ctx, value)?;
                let radix = self.number_or(ctx, radix.as_deref(), 0.0)?;
                self.intrinsic(ctx, Intrinsic::ParseInt, vec![text, radix])
            }
            Expr::ParseFloat(string) => {
                let value = self.expression(ctx, string)?;
                let text = self.display(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::ParseFloat, vec![text])
            }
            Expr::IsNaN(value) => {
                let value = self.number_coerce(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::NumberIsNaN, vec![value])
            }
            Expr::IsFinite(value) => {
                let value = self.number_coerce(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::NumberIsFinite, vec![value])
            }
            Expr::NumberIsNaN(value) => {
                let value = self.number_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::NumberIsNaN, vec![value])
            }
            Expr::NumberIsFinite(value) => {
                let value = self.number_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::NumberIsFinite, vec![value])
            }
            Expr::NumberIsInteger(value) => {
                let value = self.number_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::NumberIsInteger, vec![value])
            }
            Expr::NumberIsSafeInteger(value) => {
                let value = self.number_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::NumberIsSafeInteger, vec![value])
            }
            Expr::Uint8ArrayNew(None) => {
                let zero = self.number(ctx, 0.0);
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![zero])
            }
            Expr::Uint8ArrayNew(Some(source)) => {
                self.bytes_from(ctx, BytesCreation::Construct, source)?
            }
            Expr::Uint8ArrayFrom(source) => self.bytes_from(ctx, BytesCreation::From, source)?,
            Expr::Uint8ArrayGet { array, index } => {
                let bytes = self.typed_operand(ctx, array, Kind::Bytes, "a Uint8Array")?;
                let index = self.number_operand(ctx, index)?;
                self.intrinsic(ctx, Intrinsic::BytesGet, vec![bytes, index])
            }
            Expr::Uint8ArraySet {
                array,
                index,
                value,
            } => {
                let bytes = self.typed_operand(ctx, array, Kind::Bytes, "a Uint8Array")?;
                let index = self.number_operand(ctx, index)?;
                let value = self.number_operand(ctx, value)?;
                self.intrinsic_void(ctx, Intrinsic::BytesSet, vec![bytes, index, value]);
                value
            }
            Expr::Uint8ArrayLength(array) => {
                let bytes = self.typed_operand(ctx, array, Kind::Bytes, "a Uint8Array")?;
                self.intrinsic(ctx, Intrinsic::BytesLength, vec![bytes])
            }
            Expr::TextEncoderEncode(text) => {
                let text = self.string_operand(ctx, text)?;
                self.intrinsic(ctx, Intrinsic::TextEncode, vec![text])
            }
            Expr::TextDecoderNew {
                label,
                fatal,
                ignore_bom,
            } => self.text_decoder(ctx, label, fatal, ignore_bom)?,
            Expr::TextDecoderDecode { decoder, input } => {
                let decoder = self.typed_operand(ctx, decoder, Kind::Decoder, "a TextDecoder")?;
                self.decode(ctx, decoder, Some(input), None)?
            }
            Expr::TextDecoderEncoding(decoder) => {
                self.typed_operand(ctx, decoder, Kind::Decoder, "a TextDecoder")?;
                self.string(ctx, "utf-8")
            }
            Expr::TextDecoderFatal(decoder) | Expr::TextDecoderIgnoreBom(decoder) => {
                let decoder = self.typed_operand(ctx, decoder, Kind::Decoder, "a TextDecoder")?;
                let intrinsic = if matches!(expr, Expr::TextDecoderFatal(_)) {
                    Intrinsic::DecoderFatal
                } else {
                    Intrinsic::DecoderIgnoreBom
                };
                self.intrinsic(ctx, intrinsic, vec![decoder])
            }
            Expr::StringFromCharCode(_) | Expr::StringFromCharCodeSpread(_) => {
                bail!("fromCharCode uses UTF-16 code units; use fromCodePoint")
            }
            Expr::StringFromCodePoint(value) => {
                let value = self.number_operand(ctx, value)?;
                self.intrinsic(ctx, Intrinsic::StringFromCodePoint, vec![value])
            }
            Expr::StringCodePointAt { string, index } => {
                let value = self.string_operand(ctx, string)?;
                let index = self.number_operand(ctx, index)?;
                self.intrinsic(ctx, Intrinsic::StringCodePointAt, vec![value, index])
            }
            Expr::JsonParse(text) => {
                let text = self.string_operand(ctx, text)?;
                self.parse_json_text(ctx, text, expected)?
            }
            Expr::JsonParseTyped { text, ty, .. } => {
                let text = self.string_operand(ctx, text)?;
                let target = if let Some(int_ty) = self.shapes.integer_type(ctx, ty)
                    && int_ty.deref(ctx).width() == 64
                {
                    int_ty.into()
                } else if matches!(ty, HirType::Any | HirType::Unknown) {
                    expected.unwrap_or_else(|| JsonType::get(ctx).into())
                } else {
                    self.shapes.value_type(ctx, ty)?
                };
                let parsed = self.parse_json_text(ctx, text, Some(target))?;
                let decoded = self.fit(ctx, parsed, target)?;
                if expected != Some(target) {
                    self.bigint_view(ctx, decoded)
                } else {
                    decoded
                }
            }
            Expr::JsonStringify(value) => self.stringify(ctx, value)?,
            Expr::JsonStringifyFull(value, replacer, space) => {
                ensure!(
                    matches!(&**replacer, Expr::Null | Expr::Undefined)
                        && matches!(&**space, Expr::Null | Expr::Undefined),
                    "{JSON_CALLBACKS}"
                );
                self.stringify(ctx, value)?
            }
            Expr::JsonParseWithReviver(text, reviver)
            | Expr::JsonParseReviver { text, reviver } => {
                ensure!(
                    matches!(&**reviver, Expr::Null | Expr::Undefined),
                    "{JSON_CALLBACKS}"
                );
                let text = self.string_operand(ctx, text)?;
                self.parse_json_text(ctx, text, expected)?
            }
            Expr::JsonStringifyPretty { .. } => bail!("{JSON_CALLBACKS}"),
            Expr::MathRandom => self.intrinsic(ctx, Intrinsic::MathRandom, vec![]),
            Expr::CryptoRandomUUID => self.intrinsic(ctx, Intrinsic::RandomUuid, vec![]),
            Expr::TypeOf(value) => {
                let value = self.expression(ctx, value)?;
                self.type_of(ctx, value)?
            }
            Expr::EnvGet(name) => {
                let name = self.string(ctx, name);
                self.intrinsic(ctx, Intrinsic::EnvGet, vec![name])
            }
            Expr::PerformanceNow => self.intrinsic(ctx, Intrinsic::PerformanceNow, vec![]),
            Expr::DateNow => self.intrinsic(ctx, Intrinsic::DateNow, vec![]),
            Expr::ErrorNew(message) => {
                let message = match message {
                    Some(message) => self.string_operand(ctx, message)?,
                    None => self.string(ctx, ""),
                };
                self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, ErrorKind::Any, message))
            }
            Expr::TypeErrorNew(message)
            | Expr::RangeErrorNew(message)
            | Expr::ReferenceErrorNew(message)
            | Expr::SyntaxErrorNew(message) => {
                let kind = match expr {
                    Expr::TypeErrorNew(_) => ErrorKind::TypeError,
                    Expr::RangeErrorNew(_) => ErrorKind::RangeError,
                    Expr::ReferenceErrorNew(_) => ErrorKind::ReferenceError,
                    _ => ErrorKind::SyntaxError,
                };
                let message = self.string_operand(ctx, message)?;
                self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, kind, message))
            }
            // Literal causes keep their authored value through the shared cause conversion.
            Expr::ErrorNewWithCause { message, cause } => {
                self.error_with_cause(ctx, ErrorKind::Any, message, cause)?
            }
            Expr::ErrorNewWithOptions {
                kind: error_kind,
                message,
                options,
            } => self.error_with_options(ctx, *error_kind, message, options)?,
            Expr::ErrorMessage(error) => {
                let error = self.expression(ctx, error)?;
                ensure!(
                    kind(ctx, error)? == Kind::Exception,
                    ".message is only supported on caught exceptions and Error objects"
                );
                self.emit_value(ctx, |ctx| ExceptionMessageOp::new(ctx, error))
            }
            Expr::InstanceOf {
                expr: value, ty, ..
            } => self.instance_of(ctx, value, ty)?,
            Expr::Await(promise) => self.await_(ctx, promise)?,
            Expr::PropertyGet {
                object,
                property,
                byte_offset,
            } => self.property_get(ctx, object, property).map_err(|error| {
                self.shapes
                    .metadata
                    .locations
                    .annotate_member(error, *byte_offset)
            })?,
            Expr::PropertySet {
                object,
                property,
                value,
            } => self.set_field(ctx, object, property, value)?,
            Expr::PutValueSet {
                target, key, value, ..
            } => match key.as_ref() {
                Expr::String(field) => self.set_field(ctx, target, field, value)?,
                index => self.set_index(ctx, target, index, value)?,
            },
            Expr::New {
                class_name, args, ..
            } if class_name == "AbortController" && !self.shapes.is_user_type(class_name) => {
                ensure!(args.is_empty(), "new AbortController takes no arguments");
                self.intrinsic(ctx, Intrinsic::AbortNew, vec![])
            }
            Expr::New {
                class_name, args, ..
            } if matches!(class_name.as_str(), "Headers" | "Response" | "Request")
                && !self.shapes.is_user_type(class_name) =>
            {
                self.web_new(ctx, class_name, args)?
            }
            Expr::FetchWithOptions {
                url,
                method,
                body,
                headers,
                headers_dynamic,
                signal,
                redirect,
            } => {
                ensure!(
                    signal.is_none() && redirect.is_none(),
                    "fetch signal and redirect options are not supported yet"
                );
                self.fetch_with_options(
                    ctx,
                    url,
                    method,
                    body,
                    headers,
                    headers_dynamic.as_deref(),
                )?
            }
            Expr::NativeMethodCall {
                module,
                class_name: Some(class),
                object: None,
                method,
                args,
            } if matches!(module.as_str(), "stream" | "node:stream")
                && class == "Writable"
                && method == "toWeb" =>
            {
                self.writable_to_web(ctx, args)?
            }
            Expr::NativeMethodCall {
                object: Some(object),
                method,
                args,
                ..
            } => self.web_method(ctx, object, method, args)?,
            Expr::This => self.class_this(ctx)?,
            Expr::StaticFieldGet {
                class_name,
                field_name,
            } => self.class_static_field_get(ctx, class_name, field_name)?,
            Expr::StaticFieldSet {
                class_name,
                field_name,
                value,
            } => self.class_static_field_set(ctx, class_name, field_name, value)?,
            Expr::StaticMethodCall {
                class_name,
                method_name,
                args,
            } => self
                .class_static_method_call(ctx, class_name, method_name, args)?
                .context("a void static method call has no value")?,
            Expr::New {
                class_name,
                args,
                type_args,
                ..
            } if self.shapes.classes.contains_key(class_name.as_str()) => {
                self.class_new(ctx, class_name, args, type_args, expected)?
            }
            Expr::New {
                class_name, args, ..
            } => self.object_literal(ctx, class_name, args, expected)?,
            // `[a, b]` where a tuple is expected: its record `{ _0: a, _1: b }`.
            Expr::Array(items)
                if expected.is_some_and(|ty| {
                    TypedHandle::<RecordType>::from_handle(present_type(ctx, ty), ctx)
                        .is_ok_and(|record| record.deref(ctx).is_tuple())
                }) =>
            {
                let names: Vec<String> = (0..items.len()).map(|i| format!("_{i}")).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                self.record_literal(ctx, &names, items, expected)?
            }
            Expr::Array(items) => self.array_literal(ctx, items, expected)?,
            Expr::ArraySpread(items) => self.array_spread(ctx, items, expected)?,
            Expr::ArrayFrom(iterable) => self.array_from(ctx, iterable, None, expected)?,
            Expr::ArrayFromMapped {
                iterable, map_fn, ..
            } => self.array_from(ctx, iterable, Some(map_fn), expected)?,
            Expr::ArrayMap { array, callback } | Expr::ArrayFilter { array, callback } => {
                let array = self.expression(ctx, array)?;
                self.array_transform(
                    ctx,
                    array,
                    callback,
                    if matches!(expr, Expr::ArrayFilter { .. }) {
                        Transform::Filter
                    } else {
                        Transform::Map
                    },
                )?
            }
            Expr::ArrayFind { array, callback } | Expr::ArrayFindIndex { array, callback } => {
                let array = self.expression(ctx, array)?;
                self.array_find(
                    ctx,
                    array,
                    callback,
                    if matches!(expr, Expr::ArrayFindIndex { .. }) {
                        FindResult::Index
                    } else {
                        FindResult::Value
                    },
                    Direction::Forward,
                )?
            }
            Expr::ArraySome { array, callback } | Expr::ArrayEvery { array, callback } => {
                let array = self.expression(ctx, array)?;
                self.array_quantifier(
                    ctx,
                    array,
                    callback,
                    if matches!(expr, Expr::ArrayEvery { .. }) {
                        Quantifier::Every
                    } else {
                        Quantifier::Some
                    },
                )?
            }
            Expr::ArrayFlatMap { array, callback } => {
                let array = self.expression(ctx, array)?;
                self.array_transform(ctx, array, callback, Transform::FlatMap)?
            }
            Expr::ArraySort { array, comparator } => {
                let array = self.expression(ctx, array)?;
                let comparator =
                    (!matches!(**comparator, Expr::Undefined)).then_some(comparator.as_ref());
                self.array_sort(ctx, array, comparator, Mutation::InPlace)?
            }
            Expr::ArrayToSorted { array, comparator } => {
                let array = self.expression(ctx, array)?;
                let comparator = comparator
                    .as_deref()
                    .filter(|c| !matches!(c, Expr::Undefined));
                self.array_sort(ctx, array, comparator, Mutation::Copy)?
            }
            Expr::ArrayIncludes {
                array,
                value,
                from_index,
            }
            | Expr::ArrayIndexOf {
                array,
                value,
                from_index,
            } => {
                let array = self.expression(ctx, array)?;
                self.array_search(
                    ctx,
                    array,
                    value,
                    from_index.as_deref(),
                    if matches!(expr, Expr::ArrayIncludes { .. }) {
                        SearchResult::Includes
                    } else {
                        SearchResult::Index
                    },
                )?
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => self
                .local(*array_id)
                .and_then(|slot| self.load(ctx, slot))
                .and_then(|array| {
                    self.push_values(ctx, array, std::slice::from_ref(value.as_ref()))
                })
                .map_err(|error| self.shapes.annotate_local(error, *array_id))?,
            Expr::IndexGet { object, index } => self.index_get(ctx, object, index)?,
            Expr::IndexSet {
                object,
                index,
                value,
            } => self.set_index(ctx, object, index, value)?,
            // Perry HIR exposes no variant names, only `Debug`: show the whole node.
            other => bail!("MIR backend does not support expression {other:?} yet"),
        };
        self.narrow_primitive(ctx, expr, value)
    }

    /// A bigint literal (`0x…n`, `1_000n`), within the 64-bit transport domain.
    fn bigint_literal(&mut self, ctx: &mut Context, text: &str) -> Result<Value> {
        let text = text.trim_end_matches('n').replace('_', "");
        let (radix, digits) =
            if let Some(n) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                (16, n)
            } else if let Some(n) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
                (8, n)
            } else if let Some(n) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
                (2, n)
            } else {
                (10, text.as_str())
            };
        let magnitude = u64::from_str_radix(digits, radix)
            .context("bigint literal exceeds the 64-bit transport domain")?;
        Ok(self.emit_value(ctx, |ctx| perry_mir::BigIntConstOp::new(ctx, magnitude)))
    }

    /// `null` or `undefined` of the expected optional type (`number | null/undefined` when
    /// nothing is expected).
    fn absent_literal(
        &mut self,
        ctx: &mut Context,
        null: bool,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let ty = match expected {
            Some(ty) => ty,
            None => {
                let number = FP64Type::get(ctx).into();
                let kind = if null {
                    perry_mir::Absence::Null
                } else {
                    perry_mir::Absence::Undefined
                };
                perry_mir::OptionalType::get_with(ctx, number, kind).into()
            }
        };
        let allowed = absence(ctx, ty).is_some_and(|a| {
            if null {
                a.admits_null()
            } else {
                a.admits_undefined()
            }
        });
        ensure!(
            allowed,
            "{} is not admitted by the declared type",
            if null { "null" } else { "undefined" }
        );
        Ok(if null {
            self.emit_value(ctx, |ctx| NullOp::new(ctx, ty))
        } else {
            self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
        })
    }

    /// `local = value`, fitted to the local's type.
    pub(super) fn local_set(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        value: &'s Expr,
    ) -> Result<Value> {
        let local = self.local_binding(id)?;
        let binding = BindingType {
            carrier: self.slot_type(ctx, local.slot)?,
            integer: local.integer,
        };
        let value = self.expression_with(ctx, value, Some(binding.expected_type(ctx)))?;
        let value = self.fit_binding(ctx, value, binding)?;
        self.store(ctx, local.slot, value);
        Ok(value)
    }

    /// `++local`, `local--` and their relatives.
    pub(super) fn update(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        op: &UpdateOp,
        prefix: bool,
    ) -> Result<Value> {
        let local = self.local_binding(id)?;
        let old = self.load(ctx, local.slot)?;
        let old = self.arithmetic(ctx, old);
        ensure!(kind(ctx, old)? == Kind::Number, "++/-- needs a number");
        let one = self.number(ctx, 1.0);
        let kind = match op {
            UpdateOp::Increment => BinaryKind::Add,
            UpdateOp::Decrement => BinaryKind::Sub,
        };
        let new = self.binary(ctx, kind, old, one);
        let binding = BindingType {
            carrier: self.slot_type(ctx, local.slot)?,
            integer: local.integer,
        };
        let stored = self.fit_binding(ctx, new, binding)?;
        self.store(ctx, local.slot, stored);
        Ok(if prefix { stored } else { old })
    }

    /// `condition ? then : otherwise`, joined through a result slot.
    pub(super) fn conditional(
        &mut self,
        ctx: &mut Context,
        condition: &'s Expr,
        then_expr: &'s Expr,
        else_expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let guards = self.variant_guards.len();
        let test = self.condition(ctx, condition)?;
        let then = self.new_block(ctx, "then");
        let otherwise = self.new_block(ctx, "else");
        let join = self.new_block(ctx, "join");
        self.branch(ctx, test, then, otherwise);
        if expected.is_none()
            && matches!(then_expr, Expr::Null | Expr::Undefined)
            && !matches!(else_expr, Expr::Null | Expr::Undefined)
        {
            self.switch_to(otherwise);
            self.variant_condition(condition, false);
            let else_value = self.expression_with(ctx, else_expr, None)?;
            let else_ty = else_value.get_type(ctx);
            let then_absence = if matches!(then_expr, Expr::Null) {
                perry_mir::Absence::Null
            } else {
                perry_mir::Absence::Undefined
            };
            let ty = crate::coerce::admitting(ctx, else_ty, then_absence);
            let result = self.entry_slot(ctx, ty);
            let else_value = self.fit(ctx, else_value, ty)?;
            self.store(ctx, result, else_value);
            self.jump(ctx, join);
            self.variant_guards.truncate(guards);
            self.switch_to(then);
            self.variant_condition(condition, true);
            let then_value = self.expression_with(ctx, then_expr, Some(ty))?;
            let then_value = self.fit(ctx, then_value, ty)?;
            self.store(ctx, result, then_value);
            self.jump(ctx, join);
            self.variant_guards.truncate(guards);
            self.switch_to(join);
            return self.load(ctx, result);
        }
        let (undef_eq_path, undef_ne_path) = match condition {
            Expr::Compare {
                op: op @ (CompareOp::Eq | CompareOp::Ne),
                left,
                right,
            } => {
                let tested = if matches!(**right, Expr::Undefined) {
                    Some(&**left)
                } else if matches!(**left, Expr::Undefined) {
                    Some(&**right)
                } else {
                    None
                };
                match (op, tested.and_then(variants::path)) {
                    (CompareOp::Eq, p) => (p, None),
                    (CompareOp::Ne, p) => (None, p),
                    _ => (None, None),
                }
            }
            _ => (None, None),
        };
        let then_expected = match expected {
            Some(expected) => Some(expected),
            None if matches!(then_expr, Expr::Null | Expr::Undefined) => {
                if let Expr::LocalGet(id) = else_expr
                    && let Some(local) = self.locals.get(id)
                    && let Ok(else_ty) = self.slot_type(ctx, local.slot)
                {
                    let inner = present_type(ctx, else_ty);
                    let then_absence = if matches!(then_expr, Expr::Null) {
                        perry_mir::Absence::Null
                    } else {
                        perry_mir::Absence::Undefined
                    };
                    Some(crate::coerce::admitting(ctx, inner, then_absence))
                } else {
                    None
                }
            }
            None => None,
        };
        self.switch_to(then);
        // With an expected type both arms fit it (`cond ? value : undefined`).
        self.variant_condition(condition, true);
        let then_value = self.expression_with(ctx, then_expr, then_expected)?;
        let then_value = if undef_ne_path.is_some()
            && undef_ne_path == variants::path(then_expr)
            && !matches!(else_expr, Expr::Undefined)
        {
            self.strip_undefined(ctx, then_value)?
        } else {
            then_value
        };
        let then_value = match expected {
            Some(expected) => self.fit(ctx, then_value, expected)?,
            None => then_value,
        };
        let then_end = self.block;
        let then_ty = then_value.get_type(ctx);
        self.switch_to(otherwise);
        self.variant_guards.truncate(guards);
        self.variant_condition(condition, false);
        let else_expected = match expected {
            Some(expected) => expected,
            None if matches!(else_expr, Expr::Null | Expr::Undefined) => {
                let else_absence = if matches!(else_expr, Expr::Null) {
                    perry_mir::Absence::Null
                } else {
                    perry_mir::Absence::Undefined
                };
                crate::coerce::admitting(ctx, then_ty, else_absence)
            }
            None => then_ty,
        };
        let else_value = self.expression_with(ctx, else_expr, Some(else_expected))?;
        let else_value = if undef_eq_path.is_some()
            && undef_eq_path == variants::path(else_expr)
            && !matches!(then_expr, Expr::Undefined)
        {
            self.strip_undefined(ctx, else_value)?
        } else {
            else_value
        };
        let else_end = self.block;
        let ty = match expected {
            Some(expected) => expected,
            None => {
                let else_ty = else_value.get_type(ctx);
                if let Some(states) =
                    crate::coerce::merge_absence(absence(ctx, then_ty), absence(ctx, else_ty))
                {
                    let inner = present_type(ctx, then_ty);
                    perry_mir::OptionalType::get_with(ctx, inner, states).into()
                } else {
                    then_ty
                }
            }
        };
        // The slot is created in the entry block so it dominates both arms.
        let result = self.entry_slot(ctx, ty);
        self.switch_to(then_end);
        let then_value = self.fit(ctx, then_value, ty)?;
        self.store(ctx, result, then_value);
        self.jump(ctx, join);
        self.switch_to(else_end);
        let else_value = self.fit(ctx, else_value, ty)?;
        self.store(ctx, result, else_value);
        self.jump(ctx, join);
        self.variant_guards.truncate(guards);
        self.switch_to(join);
        self.load(ctx, result)
    }

    /// `await promise` in an async function.
    fn await_(&mut self, ctx: &mut Context, promise: &'s Expr) -> Result<Value> {
        ensure!(self.is_async, "await is only valid in async functions");
        let promise = self.expression(ctx, promise)?;
        ensure!(
            TypedHandle::<PromiseType>::from_handle(promise.get_type(ctx), ctx).is_ok(),
            "await of a non-promise value is not supported yet"
        );
        let op = AwaitOp::new(ctx, promise).map_err(|e| anyhow!("{}", e.disp(ctx)))?;
        Ok(self.append_value(ctx, &op))
    }
}
