//! Expressions: the dispatch over HIR expressions, and locals, literals and member reads.
use super::*;
use crate::lower::methods::Call;
use crate::lower::{builtins::BytesCreation, json::JSON_CALLBACKS};

mod instances;
mod members;
pub(super) use members::{Assigned, Member};

impl<'s> FunctionBuilder<'s, '_> {
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
        self.instance_operand(ctx, expr, expected, true)?
            .ok_or_else(|| diag!(codes::TYPE_MISMATCH, "a void call has no value"))
    }

    pub(super) fn source_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        expected: Option<TypeHandle>,
        required: bool,
    ) -> Result<Option<Value>> {
        if !required && self.shapes.specialization.instances.locals.is_release(expr) {
            return Ok(None);
        }
        if let Some(value) = hir_compat::non_null_assertion(expr) {
            let value = self.expression_with(ctx, value, expected)?;
            return Ok(Some(if self.is_optional(ctx, value) {
                self.emit_value(OptionalCastOp::assert_present(ctx, value), ctx)
            } else {
                value
            }));
        }
        if let Some(record) = self.shapes.specialization.instances.locals.expression(expr) {
            return match record {
                hir_compat::RecordExpr::Value(value) => self.expression_with(ctx, value, expected),
                hir_compat::RecordExpr::RequireObject(value) => {
                    let v = self.expression_with(ctx, value, expected)?;
                    self.defined(ctx, v)
                }
                hir_compat::RecordExpr::Length(source) => {
                    let source = self.expression(ctx, source)?;
                    let source = self.defined(ctx, source)?;
                    if let Some(record) = perry_mir::typed::<RecordType>(ctx, source.get_type(ctx))
                        && record.deref(ctx).kind() == RecordKind::Tuple
                    {
                        let length = record.deref(ctx).fields().to_diag(ctx)?.len();
                        Ok(self.number(ctx, length as f64))
                    } else {
                        Ok(self.array_length(ctx, source))
                    }
                }
                hir_compat::RecordExpr::Close => self.absent_literal(ctx, false, expected),
            }
            .map(Some);
        }
        if let Some(value) = self.date_expression(ctx, expr)? {
            return Ok(Some(value));
        }
        if let Some(value) = self.url_expression(ctx, expr)? {
            return Ok(Some(value));
        }
        if let Some(value) = self.collection_expression(ctx, expr, expected)? {
            return self
                .narrow_value(ctx, || crate::narrowing::path(expr), value)
                .map(Some);
        }
        if let Some((receiver, call)) = Call::expression(expr) {
            let receiver = match receiver {
                crate::lower::methods::CallReceiver::Expression(expr) => {
                    self.expression(ctx, expr)?
                }
                crate::lower::methods::CallReceiver::Local(id) => self
                    .load_local(ctx, id)
                    .map_err(|error| self.shapes.annotate_local(error, id))?,
            };
            let receiver = self.defined(ctx, receiver)?;
            return match call.lower(self, ctx, receiver, None)? {
                Some(value) => self
                    .narrow_value(ctx, || crate::narrowing::path(expr), value)
                    .map(Some),
                None if required => Err(diag!(
                    codes::TYPE_MISMATCH,
                    "method expression produces no value"
                )),
                None => Ok(None),
            };
        }
        let value = match expr {
            expr if let Some((op, inputs)) = crate::engine::builtins::hir_intrinsic(expr) => {
                use crate::engine::builtins::IntrinsicInput::*;
                let values = inputs
                    .map(|input| match input {
                        Number(expr) => self.number_operand(ctx, expr),
                        CoerceNumber(expr) => self.number_coerce(ctx, expr),
                        Required(expr, kind, label) => self.typed_operand(ctx, expr, kind, label),
                        Display(expr) => {
                            let value = self.expression(ctx, expr)?;
                            self.display(ctx, value)
                        }
                        Literal(text) => Ok(self.string(ctx, text)),
                    })
                    .collect::<Result<_>>()?;
                self.intrinsic(ctx, op, values)
            }
            Expr::PrivateGuard {
                kind: 0, object, ..
            } => self.expression_with(ctx, object, expected)?,
            Expr::ObjectSpread { parts } => self.object_spread(ctx, parts, expected, None)?,
            Expr::ObjectRest {
                object,
                exclude_keys,
            } => self.object_rest(ctx, object, exclude_keys, expected)?,
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
            Expr::ScopedTemp { id, value, body } => {
                return self.with_temp(ctx, *id, value, |this, ctx| {
                    this.instance_operand(ctx, body, expected, required)
                });
            }
            Expr::Sequence(parts) => {
                let Some((last, effects)) = parts.split_last() else {
                    return Ok(None);
                };
                if let Some(span) = self.shapes.locations.operator_span(parts) {
                    return self
                        .instance_operand(ctx, last, expected, required)
                        .map_err(|error| error.with_fallback_span(Some(span)));
                }
                for part in effects {
                    self.effect(ctx, part)?;
                }
                return self.instance_operand(ctx, last, expected, required);
            }
            Expr::Undefined if !required => return Ok(None),
            Expr::Call {
                callee,
                args,
                byte_offset,
                ..
            } => {
                let value = self
                    .call(ctx, callee, args, *byte_offset)
                    .map_err(|error| self.shapes.annotate_call(error, *byte_offset))?;
                if required && value.is_none() {
                    return Err(self.shapes.annotate_call(
                        diag!(codes::TYPE_MISMATCH, "a void call has no value"),
                        *byte_offset,
                    ));
                }
                return Ok(value);
            }
            Expr::StaticMethodCall {
                class_name,
                method_name,
                ..
            } => bail_diag!(
                codes::TYPE_MISMATCH,
                "static method {class_name}.{method_name} not found"
            ),
            Expr::ProcessExit(code) => {
                let code = match code {
                    Some(code) => {
                        let code = self.expression(ctx, code)?;
                        let code = self.arithmetic(ctx, code);
                        ensure_diag!(
                            kind(ctx, code)? == Kind::Number,
                            codes::TYPE_MISMATCH,
                            "process.exit takes a number"
                        );
                        code
                    }
                    None => self.number(ctx, 0.0),
                };
                let status = self.emit_value(ExitOp::new(ctx, code), ctx);
                self.propagate(ctx, status);
                self.after_terminator(ctx);
                return Ok(None);
            }
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
                let op = match op {
                    UpdateOp::Increment => BinaryOp::Add,
                    UpdateOp::Decrement => BinaryOp::Sub,
                };
                self.update(ctx, *id, &op, *prefix)
                    .map_err(|error| self.shapes.annotate_local(error, *id))?
            }
            Expr::Binary {
                op, left, right, ..
            } => self.binary_expression(ctx, op, left, right)?,
            Expr::Compare {
                op, left, right, ..
            } => self.comparison(ctx, op, left, right)?,
            Expr::Logical {
                op, left, right, ..
            } => self.logical(ctx, op, left, right, expected)?,
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
                ..
            } => self.conditional(ctx, condition, then_expr, else_expr, expected)?,
            Expr::Unary { op, operand } => self.unary(ctx, op, operand)?,

            Expr::ArrayPushSpread { array_id, source } => {
                let array = self.load_local(ctx, *array_id)?;
                let array = self.defined(ctx, array)?;
                let elem = self.array_element(ctx, array)?;
                self.array_argument(ctx, elem, (source, true))?
                    .append_to(self, ctx, array);
                self.array_length(ctx, array)
            }
            Expr::CallSpread { callee, args, .. } => self.array_push_spread(ctx, callee, args)?,
            Expr::MathMin(values) => self.math_extremum(ctx, false, Arguments::Plain(values))?,
            Expr::MathMax(values) => self.math_extremum(ctx, true, Arguments::Plain(values))?,
            Expr::MathMinSpread(array) => {
                self.math_extremum(ctx, false, Arguments::OneSpread(array))?
            }
            Expr::MathMaxSpread(array) => {
                self.math_extremum(ctx, true, Arguments::OneSpread(array))?
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
            Expr::Uint8ArrayNew(None) => {
                let zero = self.number(ctx, 0.0);
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![zero])
            }
            Expr::Uint8ArrayNew(Some(source)) => {
                self.bytes_from(ctx, BytesCreation::Construct, source)?
            }
            Expr::Uint8ArrayFrom(source) => self.bytes_from(ctx, BytesCreation::From, source)?,
            Expr::Uint8ArraySet {
                array,
                index,
                value,
            } => {
                let bytes = self.typed_operand(ctx, array, Kind::Bytes, "a Uint8Array")?;
                self.set_byte_index(ctx, bytes, index, value)?
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
            Expr::StringFromCharCode(_) | Expr::StringFromCharCodeSpread(_) => {
                bail_unsupported!("fromCharCode uses UTF-16 code units; use fromCodePoint")
            }
            Expr::JsonParse(text) => {
                let text = self.string_operand(ctx, text)?;
                self.parse_json_text(ctx, text, expected)?
            }
            Expr::JsonParseTyped { text, ty, .. } => {
                let text = self.string_operand(ctx, text)?;
                let target = if matches!(ty, HirType::Any | HirType::Unknown) {
                    expected.unwrap_or_else(|| JsonType::get(ctx).into())
                } else {
                    self.shapes
                        .resolved_binding(
                            self.shapes
                                .specialization
                                .instances
                                .expressions()
                                .annotation(expr)?,
                        )?
                        .expected_type(ctx)
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
                ensure_unsupported!(
                    matches!(&**space, Expr::Null | Expr::Undefined),
                    "{JSON_CALLBACKS}"
                );
                self.stringify_replaced(ctx, value, replacer)?
            }
            Expr::JsonParseWithReviver(text, reviver) => {
                ensure_unsupported!(
                    matches!(&**reviver, Expr::Null | Expr::Undefined),
                    "{JSON_CALLBACKS}"
                );
                let text = self.string_operand(ctx, text)?;
                self.parse_json_text(ctx, text, expected)?
            }
            Expr::TypeOf(value) => {
                let value = self.expression(ctx, value)?;
                self.type_of(ctx, value)?
            }
            Expr::ErrorNew(message) => {
                let message = match message {
                    Some(message) => self.string_operand(ctx, message)?,
                    None => self.string(ctx, ""),
                };
                self.emit_value(ErrorNewOp::new(ctx, ErrorKind::Any, message), ctx)
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
                self.emit_value(ErrorNewOp::new(ctx, kind, message), ctx)
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
            Expr::InstanceOf {
                expr: value, ty, ..
            } => self.instance_of(ctx, value, ty)?,
            Expr::Await(promise, _) => self.await_(ctx, promise)?,
            Expr::New {
                class_name,
                args,
                byte_offset,
                ..
            } if class_name == "DataView" && self.shapes.is_builtin(expr) => self
                .data_view(ctx, args)
                .map_err(|error| self.shapes.annotate_call(error, *byte_offset))?,
            Expr::New {
                class_name, args, ..
            } if class_name == "AbortController" && self.shapes.is_builtin(expr) => {
                ensure_diag!(
                    args.is_empty(),
                    codes::TYPE_MISMATCH,
                    "new AbortController takes no arguments"
                );
                self.intrinsic(ctx, Intrinsic::AbortNew, vec![])
            }
            Expr::New {
                class_name, args, ..
            } if matches!(class_name.as_str(), "Headers" | "Response" | "Request")
                && self.shapes.is_builtin(expr) =>
            {
                self.web_new(ctx, class_name, args)?
            }
            Expr::NativeMethodCall { args, .. } if hir_compat::is_writable_to_web(expr) => {
                self.writable_to_web(ctx, args)?
            }
            Expr::NativeMethodCall {
                object: Some(object),
                method,
                args,
                ..
            } => self.web_method(ctx, object, method, args)?,
            Expr::This => self.class_this(ctx)?,
            Expr::ThisProvenClass(class_name) => {
                self.boolean(ctx, self.is_proven_this_class(ctx, class_name))
            }
            Expr::StaticFieldGet {
                class_name,
                field_name,
            } => self.load_local(ctx, self.shapes.static_field(expr, class_name, field_name)?)?,
            Expr::StaticFieldSet {
                class_name,
                field_name,
                value,
                ..
            } => self.local_set(
                ctx,
                self.shapes.static_field(expr, class_name, field_name)?,
                value,
            )?,
            Expr::New {
                args, byte_offset, ..
            } if let Some(class) = self.shapes.symbol(expr).and_then(|s| s.instance()) => self
                .class_new(ctx, class, args)
                .map_err(|error| self.shapes.annotate_call(error, *byte_offset))?,
            Expr::New { class_name, .. } => self.object_literal(ctx, expr, class_name, expected)?,
            // `[a, b]` where a tuple is expected: its record `{ _0: a, _1: b }`.
            Expr::Array(items, _)
                if expected.is_some_and(|ty| {
                    perry_mir::typed::<RecordType>(ctx, present_type(ctx, ty))
                        .is_some_and(|record| record.deref(ctx).kind() == RecordKind::Tuple)
                }) =>
            {
                let names: Vec<String> = (0..items.len()).map(|i| format!("_{i}")).collect();
                let fields: Vec<(&str, &'s Expr)> =
                    names.iter().map(String::as_str).zip(items).collect();
                self.record_literal(ctx, &fields, expected, RecordKind::Tuple)?
            }
            Expr::Array(items, _) => self.array_literal(ctx, items, expected)?,
            Expr::ArraySpread(items) => self.array_spread(ctx, items, expected)?,
            Expr::ArrayFrom(iterable, _) => self.array_from(ctx, iterable, None, expected)?,
            Expr::ArrayFromMapped {
                iterable, map_fn, ..
            } => self.array_from(ctx, iterable, Some(map_fn), expected)?,
            Expr::Object(fields) => {
                let pairs: Vec<(&str, &'s Expr)> =
                    fields.iter().map(|(k, v)| (k.as_str(), v)).collect();
                self.record_literal(ctx, &pairs, expected, RecordKind::Object)?
            }
            _ => bail_unsupported!("this expression form is not supported yet"),
        };
        self.narrow_value(ctx, || crate::narrowing::path(expr), value)
            .map(Some)
    }

    /// A bigint literal (`0x…n`, `1_000n`), independent of WIT transport widths.
    pub(super) fn with_temp<T>(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        expr: &'s Expr,
        body: impl FnOnce(&mut Self, &mut Context) -> Result<T>,
    ) -> Result<T> {
        let value = self.expression(ctx, expr)?;
        let binding = self.initialized_local(ctx, value);
        let previous = self.locals.insert(id, binding);
        let result = body(self, ctx);
        if let Some(previous) = previous {
            self.locals.insert(id, previous);
        } else {
            self.locals.remove(&id);
        }
        result
    }

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
                    .ok_or_else(|| Diagnostic::unsupported("bigint literal too large"))?;
                APInt::from_str(digits, width, radix as u8)
                    .map_err(|_| Diagnostic::new(codes::PARSE_ERROR, "invalid bigint literal"))?
            }
        };
        let width = u32::try_from(value.bw())
            .map_err(|_| Diagnostic::unsupported("bigint literal too large"))?;
        let ty = IntegerType::get(ctx, width, Signedness::Unsigned);
        let magnitude = IntegerAttr::new(ty, value);
        Ok(self.emit_value(perry_mir::BigIntConstOp::new(ctx, magnitude), ctx))
    }

    /// Emit an absent literal in an optional or unknown type, defaulting to an optional number when unconstrained.
    pub(super) fn absent_literal(
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
        ensure_diag!(
            allowed,
            codes::TYPE_MISMATCH,
            "{} is not admitted by the declared type",
            if null { "null" } else { "undefined" }
        );
        Ok(if null {
            self.emit_value(NullOp::new(ctx, ty), ctx)
        } else {
            self.emit_value(UndefinedOp::with_type(ctx, ty), ctx)
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
        let writes = local.writes;
        let assigned = self.expression_with(ctx, value, Some(writes.expected_type(ctx)))?;
        if writes.carrier.deref(ctx).is::<JsonType>()
            && local.cell.is_none()
            && self
                .shapes
                .specialization
                .instances
                .local(id)?
                .flags
                .contains(crate::engine::types::LocalFlags::INFERRED_DEFERRED)
            && !perry_mir::JsonSchema::supports_boxing(ctx, assigned.get_type(ctx))
        {
            let name = self.shapes.local_name(id).unwrap_or("<unknown>");
            let kind = kind(ctx, assigned)?.label();
            bail_diag!(
                codes::UNSUPPORTED_TYPE,
                "local '{name}' cannot store {kind} together with its other assigned types"
            );
        }
        let result = self.assign_local(ctx, local, assigned)?;
        self.assignment_narrowing(ctx, id, (value, assigned), writes.carrier);
        Ok(result)
    }

    /// `++local`, `local--` and their relatives.
    pub(super) fn update(
        &mut self,
        ctx: &mut Context,
        id: LocalId,
        op: &BinaryOp,
        prefix: bool,
    ) -> Result<Value> {
        let local = self.local_binding(id)?;
        let old = self.load_local(ctx, id)?;
        let (old, new) = self.numeric_update(ctx, old, op)?;
        let stored = self.assign_local(ctx, local, new)?;
        self.variant_guards.assign(id);
        Ok(if prefix { stored } else { old })
    }

    fn numeric_update(
        &mut self,
        ctx: &mut Context,
        value: Value,
        op: &BinaryOp,
    ) -> Result<(Value, Value)> {
        let old = self.arithmetic(ctx, value);
        ensure_diag!(
            kind(ctx, old)? == Kind::Number,
            codes::TYPE_MISMATCH,
            "++/-- needs a number"
        );
        let op = match op {
            BinaryOp::Add => BinaryKind::Add,
            BinaryOp::Sub => BinaryKind::Sub,
            _ => bail_internal!("invalid property update"),
        };
        let one = self.number(ctx, 1.0);
        Ok((old, self.binary(ctx, op, old, one)))
    }

    pub(super) fn branch_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        solved: TypeHandle,
    ) -> Result<Value> {
        let context = matches!(
            expr,
            Expr::Null
                | Expr::Undefined
                | Expr::Array(..)
                | Expr::ArraySpread(_)
                | Expr::Object(_)
                | Expr::ObjectSpread { .. }
                | Expr::Sequence(_)
        );
        self.expression_with(ctx, expr, context.then_some(solved))
    }

    pub(super) fn conditional(
        &mut self,
        ctx: &mut Context,
        condition: &'s Expr,
        then_expr: &'s Expr,
        else_expr: &'s Expr,
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let solved = expected
            .ok_or_else(|| Diagnostic::internal("emitter omitted the conditional result type"))?;
        let test = self.condition(ctx, condition)?;
        let guard = crate::narrowing::DefinedGuard::of(condition);
        let arm = |b: &mut Self, ctx: &mut Context, truth, expr, opposite: &Expr| {
            let depth = b.variant_guards.depth();
            b.variant_guards.condition(condition, truth);
            let value = b.branch_operand(ctx, expr, solved);
            b.variant_guards.truncate(depth);
            let value = value?;
            if guard.as_ref().is_some_and(|g| g.proves(truth, expr))
                && !matches!(opposite, Expr::Undefined)
            {
                b.strip_undefined(ctx, value)
            } else {
                Ok(value)
            }
        };
        self.choose_with(
            ctx,
            test,
            |b, ctx| arm(b, ctx, true, then_expr, else_expr),
            |b, ctx| arm(b, ctx, false, else_expr, then_expr),
            |ctx, values| crate::lower::coerce::result_carrier(ctx, solved, &values),
        )
    }

    /// `await promise` in an async function.
    fn await_(&mut self, ctx: &mut Context, promise: &'s Expr) -> Result<Value> {
        ensure_unsupported!(self.is_async, "await is only valid in async functions");
        let promise = self.expression(ctx, promise)?;
        let present = present_type(ctx, promise.get_type(ctx));
        ensure_unsupported!(
            present.deref(ctx).is::<PromiseType>()
                || matches!(
                    Kind::of(ctx, present),
                    Some(Kind::Number | Kind::Boolean | Kind::String | Kind::BigInt)
                ),
            "await supports promises and primitive values"
        );
        let promise = self.as_promise_value(ctx, promise)?;
        let op = AwaitOp::new(ctx, promise).to_diag(ctx)?;
        Ok(self.emit_value(op, ctx))
    }
}
