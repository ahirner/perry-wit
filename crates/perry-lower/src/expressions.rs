//! Expressions: the dispatch over HIR expressions, and locals, literals and member reads.
use super::*;
use crate::builtins::{BytesCreation, JSON_CALLBACKS};

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
        if let Expr::Sequence(parts) = expr {
            let (last, effects) = parts.split_last().context("empty sequence expression")?;
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
        let value = match expr {
            Expr::RegExp { pattern, flags } => self.emit_value(ctx, |ctx| {
                perry_mir::regex::RegexConstOp::new(ctx, pattern, flags)
            }),
            Expr::RegExpDynamic { .. } => {
                bail!("RegExp construction is unsupported; use a bounded regex literal")
            }
            Expr::RegExpTest { regex, string } => {
                let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                let string = self.string_operand(ctx, string)?;
                self.regex_test(ctx, regex, string)
            }
            Expr::BigInt(text) => self.bigint_literal(ctx, text)?,
            Expr::BigIntCoerce(expr) => self.bigint_coerce(ctx, expr)?,
            Expr::NumberCoerce(expr) => self.number_coerce(ctx, expr)?,
            Expr::StringCoerce(expr) | Expr::TemplateStringCoerce(expr) => {
                let value = self.expression(ctx, expr)?;
                self.display(ctx, value)?
            }
            Expr::Number(value) => self.number(ctx, *value),
            Expr::Integer(value) => self.number(ctx, *value as f64),
            Expr::Bool(value) => self.boolean(ctx, *value),
            Expr::String(text) => self.string(ctx, text),
            Expr::LocalGet(id) => {
                let slot = self.local(*id)?;
                self.load(ctx, slot)?
            }
            Expr::Null | Expr::Undefined => {
                self.absent_literal(ctx, matches!(expr, Expr::Null), expected)?
            }
            Expr::LocalSet(id, value) => self.local_set(ctx, *id, value)?,
            Expr::Update { id, op, prefix } => self.update(ctx, *id, op, *prefix)?,
            Expr::PropertyUpdate {
                object,
                property,
                op,
                prefix,
                ..
            } => self.property_update(ctx, object, property, op, *prefix)?,
            Expr::Binary { op, left, right } => self.binary_expression(ctx, op, left, right)?,
            Expr::Compare { op, left, right } => self.comparison(ctx, op, left, right)?,
            Expr::Logical { op, left, right } => self.logical(ctx, op, left, right)?,
            Expr::Conditional {
                condition,
                else_expr,
                ..
            } if matches!(&**condition, Expr::LocalGet(guard) if self.false_guards.contains(guard)) => {
                self.expression(ctx, else_expr)?
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
            Expr::Call { callee, args, .. } => self
                .call(ctx, callee, args)?
                .context("a void call has no value")?,
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
                self.intrinsic(ctx, Intrinsic::JsonParse, vec![text])
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
                self.intrinsic(ctx, Intrinsic::JsonParse, vec![text])
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
            Expr::ObjectKeys(object) if matches!(**object, Expr::ProcessEnv) => {
                self.intrinsic(ctx, Intrinsic::EnvKeys, vec![])
            }
            Expr::ObjectKeys(object) => {
                let object = self.expression(ctx, object)?;
                ensure!(
                    kind(ctx, object)? == Kind::Json,
                    "Object.keys requires a JSON snapshot"
                );
                self.intrinsic(ctx, Intrinsic::JsonKeys, vec![object])
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
                object, property, ..
            } => self.property_get(ctx, object, property)?,
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
            Expr::New {
                class_name, args, ..
            } => self.object_literal(ctx, class_name, args, expected)?,
            // `[a, b]` where a tuple is expected: its record `{ _0: a, _1: b }`.
            Expr::Array(items)
                if expected.is_some_and(|ty| {
                    TypedHandle::<RecordType>::from_handle(
                        optional_value(ctx, ty).unwrap_or(ty),
                        ctx,
                    )
                    .is_ok_and(|record| record.deref(ctx).is_tuple())
                }) =>
            {
                let names: Vec<String> = (0..items.len()).map(|i| format!("_{i}")).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                self.record_literal(ctx, &names, items, expected)?
            }
            Expr::Array(items) => self.array_literal(ctx, items, expected)?,
            Expr::ArraySpread(items) => self.array_spread(ctx, items, expected)?,
            Expr::ArrayMap { array, callback } | Expr::ArrayFilter { array, callback } => {
                let array = self.expression(ctx, array)?;
                self.array_transform(
                    ctx,
                    array,
                    callback,
                    matches!(expr, Expr::ArrayFilter { .. }),
                )?
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
                    matches!(expr, Expr::ArrayIncludes { .. }),
                )?
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => self.array_push(ctx, *array_id, value)?,
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
        Ok(self.emit_value(ctx, |ctx| {
            perry_mir::integers::BigIntConstOp::new(ctx, magnitude)
        }))
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
                    perry_mir::attrs::Absence::Null
                } else {
                    perry_mir::attrs::Absence::Undefined
                };
                perry_mir::types::OptionalType::get_with(ctx, number, kind).into()
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
    fn local_set(&mut self, ctx: &mut Context, id: LocalId, value: &'s Expr) -> Result<Value> {
        let slot = self.local(id)?;
        let slot_type = self.slot_type(ctx, slot)?;
        let value = self.expression_with(ctx, value, Some(slot_type))?;
        let value = self.fit(ctx, value, slot_type)?;
        self.store(ctx, slot, value);
        Ok(value)
    }

    /// `++local`, `local--` and their relatives.
    fn update(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        op: &UpdateOp,
        prefix: bool,
    ) -> Result<Value> {
        let slot = self.local(id)?;
        let old = self.load(ctx, slot)?;
        let old = self.arithmetic(ctx, old);
        ensure!(kind(ctx, old)? == Kind::Number, "++/-- needs a number");
        let one = self.number(ctx, 1.0);
        let kind = match op {
            UpdateOp::Increment => BinaryKind::Add,
            UpdateOp::Decrement => BinaryKind::Sub,
        };
        let new = self.binary(ctx, kind, old, one);
        let slot_type = self.slot_type(ctx, slot)?;
        let stored = self.fit(ctx, new, slot_type)?;
        self.store(ctx, slot, stored);
        Ok(if prefix { new } else { old })
    }

    /// `++object.field` and its relatives, on a numeric record field.
    fn property_update(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        property: &str,
        op: &BinaryOp,
        prefix: bool,
    ) -> Result<Value> {
        let object = self.expression(ctx, object)?;
        let object = self.defined(ctx, object)?;
        let get = FieldGetOp::new(ctx, object, property.to_string())
            .map_err(|_| anyhow!("property update needs a declared numeric field"))?;
        let previous = self.append_value(ctx, &get);
        let field_type = previous.get_type(ctx);
        let old = self.arithmetic(ctx, previous);
        ensure!(
            kind(ctx, old)? == Kind::Number,
            "property update needs a numeric field"
        );
        let one = self.number(ctx, 1.0);
        let kind = match op {
            BinaryOp::Add => BinaryKind::Add,
            BinaryOp::Sub => BinaryKind::Sub,
            _ => bail!("invalid property update"),
        };
        let next = self.binary(ctx, kind, old, one);
        let stored = self.fit(ctx, next, field_type)?;
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, object, property.to_string(), stored)
        });
        Ok(if prefix { next } else { old })
    }

    /// `condition ? then : otherwise`, joined through a result slot.
    fn conditional(
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
        self.switch_to(then);
        // With an expected type both arms fit it (`cond ? value : undefined`).
        self.variant_condition(condition, true);
        let then_value = self.expression_with(ctx, then_expr, expected)?;
        let then_value = match expected {
            Some(expected) => self.fit(ctx, then_value, expected)?,
            None => then_value,
        };
        let ty = then_value.get_type(ctx);
        // The slot is created in the entry block so it dominates both arms.
        let result = self.entry_slot(ctx, ty);
        self.store(ctx, result, then_value);
        self.jump(ctx, join);
        self.switch_to(otherwise);
        self.variant_guards.truncate(guards);
        self.variant_condition(condition, false);
        let else_value = self.expression_with(ctx, else_expr, Some(ty))?;
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

    /// `object.property`, by the kind of `object`.
    fn property_get(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        property: &str,
    ) -> Result<Value> {
        let value = self.expression(ctx, object)?;
        // Property access on `undefined` throws TypeError, in ECMAScript and here.
        let value = self.defined(ctx, value)?;
        Ok(match (kind(ctx, value)?, property) {
            (Kind::Url, field) => self.url_field(ctx, value, field)?,
            (Kind::Decoder, "encoding") => self.string(ctx, "utf-8"),
            (Kind::Decoder, "fatal" | "ignoreBOM") => {
                let intrinsic = if property == "fatal" {
                    Intrinsic::DecoderFatal
                } else {
                    Intrinsic::DecoderIgnoreBom
                };
                self.intrinsic(ctx, intrinsic, vec![value])
            }
            (Kind::String, "length") => {
                let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, value));
                self.emit_value(ctx, |ctx| {
                    ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                })
            }
            (Kind::Array, "length") => {
                let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, value));
                self.emit_value(ctx, |ctx| {
                    ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                })
            }
            (Kind::Bytes, "buffer") => self.intrinsic(ctx, Intrinsic::BytesBuffer, vec![value]),
            (Kind::Bytes, "byteOffset") => self.intrinsic(ctx, Intrinsic::BytesOffset, vec![value]),
            (Kind::ByteBuffer, "byteLength") => {
                self.intrinsic(ctx, Intrinsic::BufferLength, vec![value])
            }
            (Kind::Bytes, "length" | "byteLength") => {
                self.intrinsic(ctx, Intrinsic::BytesLength, vec![value])
            }
            (Kind::Json, "length") => self.intrinsic(ctx, Intrinsic::JsonLength, vec![value]),
            (Kind::Json, name) => {
                let name = self.string(ctx, name);
                self.intrinsic(ctx, Intrinsic::JsonMember, vec![value, name])
            }
            (Kind::Settled, field) => {
                let field = match field {
                    "status" => SettledField::Status,
                    "value" => SettledField::Value,
                    "reason" => SettledField::Reason,
                    other => bail!("settled results have no property {other}"),
                };
                let op =
                    SettledFieldOp::new(ctx, field, value).context("expected a settled result")?;
                self.append_value(ctx, &op)
            }
            (
                Kind::Headers
                | Kind::Response
                | Kind::Request
                | Kind::Writable
                | Kind::Writer
                | Kind::Readable
                | Kind::Reader
                | Kind::ByobReader
                | Kind::ReadResult,
                name,
            ) => self.web_member(ctx, value, name, None)?,
            (Kind::Instant, "epochMilliseconds") => {
                self.intrinsic(ctx, Intrinsic::InstantMs, vec![value])
            }
            (Kind::PlainDate | Kind::PlainDateTime, field) => {
                self.temporal_field(ctx, value, field)?
            }
            (Kind::Abort, "signal") => value,
            (Kind::Abort, "aborted") => self.intrinsic(ctx, Intrinsic::AbortAborted, vec![value]),
            (Kind::Stats, "size") => self.intrinsic(ctx, Intrinsic::StatsSize, vec![value]),
            (Kind::Exception, "cause") => {
                self.intrinsic(ctx, Intrinsic::ExceptionCause, vec![value])
            }
            (Kind::Exception, "name") => self.intrinsic(ctx, Intrinsic::ExceptionName, vec![value]),
            (Kind::Exception, "message") => {
                self.emit_value(ctx, |ctx| ExceptionMessageOp::new(ctx, value))
            }
            (Kind::Variant, field) => self.variant_member(ctx, object, value, field)?,
            (Kind::Record, field) => {
                let op = FieldGetOp::new(ctx, value, field.to_string()).map_err(|_| {
                    anyhow!("property {field} does not exist on this shape (subset rule R1)")
                })?;
                self.append_value(ctx, &op)
            }
            (_, property) => bail!("property {property} is not supported on this type yet"),
        })
    }

    /// `object[index]` on arrays, strings, bytes, JSON values and records (literal keys).
    fn index_get(&mut self, ctx: &mut Context, object: &'s Expr, index: &'s Expr) -> Result<Value> {
        let array = self.expression(ctx, object)?;
        let array = self.defined(ctx, array)?;
        match kind(ctx, array)? {
            Kind::String => {
                let index = self.number_operand(ctx, index)?;
                Ok(self.intrinsic(ctx, Intrinsic::StringIndex, vec![array, index]))
            }
            Kind::Bytes => {
                let index = self.number_operand(ctx, index)?;
                Ok(self.intrinsic(ctx, Intrinsic::BytesGet, vec![array, index]))
            }
            Kind::Json => {
                let index = self.expression(ctx, index)?;
                let index = self.defined(ctx, index)?;
                let intrinsic = match kind(ctx, index)? {
                    Kind::String => Intrinsic::JsonMember,
                    Kind::Number => Intrinsic::JsonIndex,
                    _ => bail!("JSON lookup requires a string or number key"),
                };
                Ok(self.intrinsic(ctx, intrinsic, vec![array, index]))
            }
            Kind::Record => {
                let field = match index {
                    Expr::String(name) => name.clone(),
                    Expr::Number(n) if n.fract() == 0.0 && *n >= 0.0 => {
                        format!("_{}", *n as u64)
                    }
                    Expr::Integer(n) if *n >= 0 => format!("_{n}"),
                    _ => bail!(
                        "closed records require literal keys; tuples require literal positions"
                    ),
                };
                let op = FieldGetOp::new(ctx, array, field.clone())
                    .map_err(|_| anyhow!("record has no field {field}"))?;
                Ok(self.append_value(ctx, &op))
            }
            Kind::Array => {
                let index = self.index(ctx, index)?;
                let op = ArrayGetOp::new(ctx, array, index)?;
                Ok(self.append_value(ctx, &op))
            }
            _ => bail!("indexing is only supported on arrays so far"),
        }
    }
}
