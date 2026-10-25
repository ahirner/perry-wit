//! Expressions: the dispatch over HIR expressions, and locals, literals and member reads.
use super::*;
use crate::methods::Call;
use crate::{builtins::BytesCreation, json::JSON_CALLBACKS};

mod members;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn expression(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        self.expression_with(ctx, expr, None)
    }

    /// Lower `expr`, using `expected` to type literals whose own type is not determined by their
    /// operands (such as empty arrays or object literals widened to a declared shape).
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
                    let source = self.defined(ctx, source)?;
                    if let Ok(record) =
                        TypedHandle::<RecordType>::from_handle(source.get_type(ctx), ctx)
                        && record.deref(ctx).kind() == RecordKind::Tuple
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
            if let [Expr::String(token), _] = parts.as_slice()
                && let Some(span) = self.shapes.metadata.locations.operator_span(token)
            {
                return self
                    .expression_with(ctx, last, expected)
                    .map_err(|error| crate::diagnostics::annotate(error, Some(span)));
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
        if let Some((receiver, call)) = Call::expression(expr) {
            let receiver = match receiver {
                crate::methods::CallReceiver::Expression(expr) => self.expression(ctx, expr)?,
                crate::methods::CallReceiver::Local(id) => self
                    .local(id)
                    .and_then(|slot| self.load(ctx, slot))
                    .map_err(|error| self.shapes.annotate_local(error, id))?,
            };
            let receiver = self.defined(ctx, receiver)?;
            let value = call
                .lower(self, ctx, receiver)?
                .context("method expression produces no value")?;
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
            Expr::MathImul(left, right) => {
                let left = self.number_operand(ctx, left)?;
                let right = self.number_operand(ctx, right)?;
                self.intrinsic(ctx, Intrinsic::MathImul, vec![left, right])
            }
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
            Expr::TextEncoderNew => self.intrinsic(ctx, Intrinsic::EncoderNew, vec![]),
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
                } else if perry_frontend::contextual_type(ty).is_none() {
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
                    matches!(&**space, Expr::Null | Expr::Undefined),
                    "{JSON_CALLBACKS}"
                );
                self.stringify_replaced(ctx, value, replacer)?
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
                crate::diagnostics::annotate(
                    error,
                    self.shapes.metadata.locations.member_span(*byte_offset),
                )
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
            } if self.shapes.classes.named.contains_key(class_name.as_str()) => {
                self.class_new(ctx, class_name, args, type_args, expected)?
            }
            Expr::New {
                class_name, args, ..
            } => self.object_literal(ctx, class_name, args, expected)?,
            // `[a, b]` where a tuple is expected: its record `{ _0: a, _1: b }`.
            Expr::Array(items, _)
                if expected.is_some_and(|ty| {
                    TypedHandle::<RecordType>::from_handle(present_type(ctx, ty), ctx)
                        .is_ok_and(|record| record.deref(ctx).kind() == RecordKind::Tuple)
                }) =>
            {
                let names: Vec<String> = (0..items.len()).map(|i| format!("_{i}")).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                self.record_literal(ctx, &names, items, expected, RecordKind::Tuple)?
            }
            Expr::Array(items, _) => self.array_literal(ctx, items, expected)?,
            Expr::ArraySpread(items) => self.array_spread(ctx, items, expected)?,
            Expr::ArrayFrom(iterable, _) => self.array_from(ctx, iterable, None, expected)?,
            Expr::ArrayFromMapped {
                iterable, map_fn, ..
            } => self.array_from(ctx, iterable, Some(map_fn), expected)?,
            Expr::IndexGet { object, index } => self.index_get(ctx, object, index)?,
            Expr::IndexSet {
                object,
                index,
                value,
            } => self.set_index(ctx, object, index, value)?,
            other => bail!("MIR backend does not support expression {other:?} yet"),
        };
        self.narrow_primitive(ctx, expr, value)
    }

    /// A bigint literal (`0x…n`, `1_000n`), independent of WIT transport widths.
    fn bigint_literal(&mut self, ctx: &mut Context, text: &str) -> Result<Value> {
        use pliron::utils::apint::{APInt, bw};
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
        let value = match u64::from_str_radix(digits, radix) {
            Ok(value) => APInt::from_u64(value, bw(64)),
            Err(_) => {
                let width = digits
                    .len()
                    .checked_mul(4)
                    .context("bigint literal too large")?;
                APInt::from_str(digits, width, radix as u8).context("invalid bigint literal")?
            }
        };
        let width = u32::try_from(value.bw()).context("bigint literal too large")?;
        let ty = IntegerType::get(ctx, width, Signedness::Unsigned);
        let magnitude = IntegerAttr::new(ty, value);
        Ok(self.emit_value(ctx, |ctx| perry_mir::BigIntConstOp::new(ctx, magnitude)))
    }

    /// Emit an absent literal in an optional or unknown type, defaulting to an optional number when unconstrained.
    fn absent_literal(
        &mut self,
        ctx: &mut Context,
        null: bool,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        if expected.is_some_and(|ty| ty.deref(ctx).is::<JsonType>()) {
            let intrinsic = if null {
                Intrinsic::JsonNull
            } else {
                Intrinsic::JsonUndefined
            };
            return Ok(self.intrinsic(ctx, intrinsic, vec![]));
        }
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
        let test = self.condition(ctx, condition)?;
        let then = self.new_block(ctx, "then");
        let otherwise = self.new_block(ctx, "else");
        self.branch(ctx, test, then, otherwise);
        let reverse = expected.is_none()
            && matches!(then_expr, Expr::Null | Expr::Undefined)
            && !matches!(else_expr, Expr::Null | Expr::Undefined);
        let arms = if reverse {
            [(otherwise, false, else_expr), (then, true, then_expr)]
        } else {
            [(then, true, then_expr), (otherwise, false, else_expr)]
        };
        let defined_path = match condition {
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
                tested
                    .and_then(variants::path)
                    .map(|path| (*op == CompareOp::Ne, path))
            }
            _ => None,
        };
        let lower_arm = |b: &mut Self,
                         ctx: &mut Context,
                         (block, truth, expr),
                         opposite: &Expr,
                         hint|
         -> Result<_> {
            b.switch_to(block);
            let guards = b.variant_guards.len();
            b.variant_condition(condition, truth);
            let value = b.expression_with(ctx, expr, hint);
            b.variant_guards.truncate(guards);
            let value = value?;
            let value = if defined_path.as_ref().is_some_and(|(defined_truth, path)| {
                *defined_truth == truth && Some(path) == variants::path(expr).as_ref()
            }) && !matches!(opposite, Expr::Undefined)
            {
                b.strip_undefined(ctx, value)?
            } else {
                value
            };
            Ok((b.block, [value]))
        };
        let left = lower_arm(self, ctx, arms[0], arms[1].2, expected)?;
        let hint = expected.or_else(|| {
            Some(crate::coerce::expression_hint(
                ctx,
                arms[1].2,
                left.1[0].get_type(ctx),
            ))
        });
        let right = lower_arm(self, ctx, arms[1], arms[0].2, hint)?;
        let left_ty = left.1[0].get_type(ctx);
        let right_ty = right.1[0].get_type(ctx);
        let ty =
            expected.unwrap_or_else(|| crate::coerce::merged_type(ctx, left_ty, left_ty, right_ty));
        let [value] = self.merge_branches(ctx, [ty], [left, right])?;
        Ok(value)
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
