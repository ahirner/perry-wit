//! Value kinds and the ECMAScript conversions between typed MIR values.
use super::*;

pub(super) fn merge_absence(
    left: Option<perry_mir::attrs::Absence>,
    right: Option<perry_mir::attrs::Absence>,
) -> Option<perry_mir::attrs::Absence> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) if left == right => Some(left),
        (Some(_), Some(_)) => Some(perry_mir::attrs::Absence::Both),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    BigInt,
    Url,
    SearchParams,
    Number,
    Boolean,
    String,
    Record,
    Variant,
    NumberString,
    Resource,
    Array,
    Exception,
    Bytes,
    ByteBuffer,
    ByobReader,
    Json,
    Date,
    Decoder,
    Regex,
    Settled,
    Stats,
    Headers,
    Response,
    Request,
    Writable,
    Writer,
    Readable,
    Reader,
    ReadResult,
    Incoming,
    Abort,
    Instant,
    PlainDate,
    PlainDateTime,
}

impl Kind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Kind::BigInt => "bigint",
            Kind::Url => "URL",
            Kind::SearchParams => "URLSearchParams",
            Kind::Number => "number",
            Kind::Boolean => "boolean",
            Kind::String => "string",
            Kind::Record => "record",
            Kind::Variant => "variant",
            Kind::NumberString => "number | string",
            Kind::Resource => "WIT resource",
            Kind::Array => "array",
            Kind::Exception => "Error",
            Kind::Bytes => "Uint8Array",
            Kind::ByteBuffer => "ArrayBuffer",
            Kind::ByobReader => "ReadableStreamBYOBReader",
            Kind::Json => "JSON value",
            Kind::Date => "Date",
            Kind::Decoder => "TextDecoder",
            Kind::Regex => "RegExp",
            Kind::Settled => "PromiseSettlementResult",
            Kind::Stats => "Stats",
            Kind::Headers => "Headers",
            Kind::Response => "Response",
            Kind::Request => "Request",
            Kind::Writable => "WritableStream",
            Kind::Writer => "WritableStreamDefaultWriter",
            Kind::Readable => "ReadableStream",
            Kind::Reader => "ReadableStreamDefaultReader",
            Kind::ReadResult => "ReadableStreamReadResult",
            Kind::Incoming => "IncomingRequest",
            Kind::Abort => "AbortController",
            Kind::Instant => "Temporal.Instant",
            Kind::PlainDate => "Temporal.PlainDate",
            Kind::PlainDateTime => "Temporal.PlainDateTime",
        }
    }
}

pub(super) fn kind(ctx: &Context, value: Value) -> Result<Kind> {
    let ty = value.get_type(ctx);
    let ty = ty.deref(ctx);
    if ty.is::<perry_mir::types::BigIntType>() {
        Ok(Kind::BigInt)
    } else if ty.is::<FP64Type>() {
        Ok(Kind::Number)
    } else if ty.is::<StringType>() {
        Ok(Kind::String)
    } else if ty.is::<perry_mir::types::ResourceType>() {
        Ok(Kind::Resource)
    } else if ty.is::<perry_mir::types::NumberStringType>() {
        Ok(Kind::NumberString)
    } else if ty.is::<perry_mir::types::VariantType>() {
        Ok(Kind::Variant)
    } else if ty.is::<RecordType>() {
        Ok(Kind::Record)
    } else if ty.is::<ArrayType>() {
        Ok(Kind::Array)
    } else if ty.is::<ExceptionType>() {
        Ok(Kind::Exception)
    } else if ty.is::<BytesType>() {
        Ok(Kind::Bytes)
    } else if ty.is::<perry_mir::types::UrlType>() {
        Ok(Kind::Url)
    } else if ty.is::<perry_mir::types::SearchParamsType>() {
        Ok(Kind::SearchParams)
    } else if ty.is::<JsonType>() {
        Ok(Kind::Json)
    } else if ty.is::<perry_mir::types::RegexType>() {
        Ok(Kind::Regex)
    } else if ty.is::<perry_mir::types::DecoderType>() {
        Ok(Kind::Decoder)
    } else if ty.is::<DateType>() {
        Ok(Kind::Date)
    } else if ty.is::<SettledType>() {
        Ok(Kind::Settled)
    } else if ty.is::<StatsType>() {
        Ok(Kind::Stats)
    } else if ty.is::<HeadersType>() {
        Ok(Kind::Headers)
    } else if ty.is::<ResponseType>() {
        Ok(Kind::Response)
    } else if ty.is::<RequestType>() {
        Ok(Kind::Request)
    } else if ty.is::<WritableType>() {
        Ok(Kind::Writable)
    } else if ty.is::<WriterType>() {
        Ok(Kind::Writer)
    } else if ty.is::<ReadableType>() {
        Ok(Kind::Readable)
    } else if ty.is::<perry_mir::types::ByteBufferType>() {
        Ok(Kind::ByteBuffer)
    } else if ty.is::<perry_mir::types::ByobReaderType>() {
        Ok(Kind::ByobReader)
    } else if ty.is::<ReaderType>() {
        Ok(Kind::Reader)
    } else if ty.is::<InstantType>() {
        Ok(Kind::Instant)
    } else if ty.is::<PlainDateType>() {
        Ok(Kind::PlainDate)
    } else if ty.is::<PlainDateTimeType>() {
        Ok(Kind::PlainDateTime)
    } else if ty.is::<AbortType>() {
        Ok(Kind::Abort)
    } else if ty.is::<IncomingType>() {
        Ok(Kind::Incoming)
    } else if ty.is::<ReadResultType>() {
        Ok(Kind::ReadResult)
    } else if ty
        .downcast_ref::<IntegerType>()
        .is_some_and(|int| int.width() == 64)
    {
        Ok(Kind::BigInt)
    } else if ty
        .downcast_ref::<IntegerType>()
        .is_some_and(|int| int.width() == 1)
    {
        Ok(Kind::Boolean)
    } else {
        bail!("unsupported value type in expression")
    }
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn is_optional(&self, ctx: &Context, value: Value) -> bool {
        optional_value(ctx, value.get_type(ctx)).is_some()
    }

    /// ECMAScript coercion of `T | undefined` for arithmetic, ordering and concatenation:
    /// `undefined` becomes `NaN` or `"undefined"`. Other values pass through.
    pub(super) fn arithmetic(&mut self, ctx: &mut Context, value: Value) -> Value {
        if value.get_type(ctx).deref(ctx).is::<JsonType>() {
            return self.intrinsic(ctx, Intrinsic::JsonToNumber, vec![value]);
        }
        let Some(inner) = optional_value(ctx, value.get_type(ctx)) else {
            return value;
        };
        if inner.deref(ctx).is::<pliron::builtin::types::FP64Type>() {
            self.emit_value(ctx, |ctx| ToNumberOp::new(ctx, value))
        } else if inner.deref(ctx).is::<StringType>() {
            self.emit_value(ctx, |ctx| ToStringOp::new(ctx, value))
        } else {
            value
        }
    }

    /// The defined value of `T | undefined`; throws `TypeError` on `undefined`.
    pub(super) fn defined(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if !self.is_optional(ctx, value) {
            return Ok(value);
        }
        let op = UnwrapOp::new(ctx, value)?;
        Ok(self.append_value(ctx, &op))
    }

    /// View `value` as `T | undefined`.
    pub(super) fn optional(&mut self, ctx: &mut Context, value: Value) -> Value {
        if self.is_optional(ctx, value) {
            value
        } else {
            self.emit_value(ctx, |ctx| WrapOp::new(ctx, value))
        }
    }

    /// Convert `value` to the type of a typed boundary: wrap into `T | undefined`, or unwrap
    /// (throwing `TypeError` on `undefined`) where `T` is required.
    pub(super) fn fit(
        &mut self,
        ctx: &mut Context,
        value: Value,
        target: TypeHandle,
    ) -> Result<Value> {
        let ty = value.get_type(ctx);
        let wanted = optional_value(ctx, target).unwrap_or(target);
        if wanted.deref(ctx).is::<perry_mir::types::NumberStringType>()
            && (ty.deref(ctx).is::<FP64Type>() || ty.deref(ctx).is::<StringType>())
        {
            let tag = if ty.deref(ctx).is::<FP64Type>() {
                "number"
            } else {
                "string"
            };
            let boxed = self.emit_value(ctx, |ctx| {
                perry_mir::variants::VariantNewOp::new(ctx, wanted, tag, Some(value))
            });
            return self.fit(ctx, boxed, target);
        }
        if target.deref(ctx).is::<perry_mir::types::ByteInputType>() {
            let convert = if ty.deref(ctx).is::<StringType>() {
                Some(Intrinsic::StringByteInput)
            } else if ty.deref(ctx).is::<BytesType>() {
                Some(Intrinsic::BytesByteInput)
            } else {
                None
            };
            if let Some(convert) = convert {
                return Ok(self.intrinsic(ctx, convert, vec![value]));
            }
        }
        if ty.deref(ctx).is::<JsonType>() && ty != target {
            if let Some((inner, states)) = optional_parts(ctx, target) {
                let convert = |b: &mut Self, ctx: &mut Context| {
                    let converted = b.fit(ctx, value, inner)?;
                    b.fit(ctx, converted, target)
                };
                let null_or_value = |b: &mut Self, ctx: &mut Context| {
                    if states.admits_null() {
                        let null = b.intrinsic(ctx, Intrinsic::JsonIsNull, vec![value]);
                        b.choose(
                            ctx,
                            target,
                            null,
                            |b, ctx| Ok(b.emit_value(ctx, |ctx| NullOp::new(ctx, target))),
                            convert,
                        )
                    } else {
                        convert(b, ctx)
                    }
                };
                return if states.admits_undefined() {
                    let undefined = self.intrinsic(ctx, Intrinsic::JsonIsUndefined, vec![value]);
                    self.choose(
                        ctx,
                        target,
                        undefined,
                        |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, target))),
                        null_or_value,
                    )
                } else {
                    null_or_value(self, ctx)
                };
            }
            let intrinsic = if target.deref(ctx).is::<FP64Type>() {
                Intrinsic::JsonRequireNumber
            } else if target.deref(ctx).is::<StringType>() {
                Intrinsic::JsonToString
            } else if self.is_bool_type(ctx, target) {
                Intrinsic::JsonToBool
            } else if let Some(int) = target.deref(ctx).downcast_ref::<IntegerType>()
                && int.width() == 64
            {
                if int.is_signed() {
                    Intrinsic::JsonToI64
                } else {
                    Intrinsic::JsonToU64
                }
            } else {
                return self.decode_aggregate(ctx, value, target);
            };
            return Ok(self.intrinsic(ctx, intrinsic, vec![value]));
        }
        let thrown = optional_value(ctx, ty).unwrap_or(ty);
        let wanted = optional_value(ctx, target).unwrap_or(target);
        if thrown.deref(ctx).is::<ExceptionType>() && thrown != wanted {
            // A thrown value (a rejection's `reason`) used as a number or string must be one.
            let intrinsic = if wanted.deref(ctx).is::<FP64Type>() {
                Intrinsic::ExceptionToNumber
            } else if wanted.deref(ctx).is::<StringType>() {
                Intrinsic::ExceptionToString
            } else if wanted.deref(ctx).is::<IntegerType>() {
                Intrinsic::ExceptionToBool
            } else {
                bail!("thrown values require a supported scalar type")
            };
            let exception = self.defined(ctx, value)?;
            let converted = self.intrinsic(ctx, intrinsic, vec![exception]);
            return self.fit(ctx, converted, target);
        }
        if thrown.deref(ctx).is::<perry_mir::types::BigIntType>()
            && wanted
                .deref(ctx)
                .downcast_ref::<IntegerType>()
                .is_some_and(|int| int.width() == 64)
        {
            let defined = self.defined(ctx, value)?;
            let casted = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, defined, wanted));
            return self.fit(ctx, casted, target);
        }
        if thrown
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|int| int.width() == 64)
            && wanted.deref(ctx).is::<perry_mir::types::BigIntType>()
        {
            let defined = self.defined(ctx, value)?;
            let casted = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, defined, wanted));
            return self.fit(ctx, casted, target);
        }
        if ty == target {
            Ok(value)
        } else if optional_value(ctx, target) == Some(ty) {
            Ok(self.emit_value(ctx, |ctx| WrapOp::with_type(ctx, value, target)))
        } else if optional_value(ctx, target).is_some()
            && optional_value(ctx, target) == optional_value(ctx, ty)
        {
            Ok(self.emit_value(ctx, |ctx| OptionalCastOp::new(ctx, value, target)))
        } else if optional_value(ctx, ty) == Some(target) {
            self.defined(ctx, value)
        } else if perry_mir::views::can_widen(ctx, ty, target) {
            Ok(self.emit_value(ctx, |ctx| {
                perry_mir::views::AggregateViewOp::new(ctx, value, target)
            }))
        } else {
            let source = optional_value(ctx, ty).unwrap_or(ty);
            let wanted = optional_value(ctx, target).unwrap_or(target);
            let code =
                if source.deref(ctx).is::<RecordType>() && wanted.deref(ctx).is::<RecordType>() {
                    DiagnosticCode::IncompatibleRecordLayout
                } else if absence(ctx, ty) != absence(ctx, target) {
                    DiagnosticCode::AbsenceMismatch
                } else {
                    DiagnosticCode::UnsupportedType
                };
            Err(PerryDiagnostic::error(
                code,
                format!(
                    "value type {} differs from the expected type {}",
                    ty.disp(ctx),
                    target.disp(ctx)
                ),
            )
            .into())
        }
    }

    /// An expression that must be a string (`undefined` throws `TypeError`).
    pub(super) fn string_operand(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.defined(ctx, value)?;
        ensure!(
            kind(ctx, value)? == Kind::String,
            "a string is required here"
        );
        Ok(value)
    }

    /// An ECMAScript number used as an array index (R8: reads miss with `undefined`).
    pub(super) fn index(&mut self, ctx: &mut Context, index: &'s Expr) -> Result<Value> {
        let index = self.expression(ctx, index)?;
        let index = self.arithmetic(ctx, index);
        ensure!(
            kind(ctx, index)? == Kind::Number,
            "array indices must be numbers"
        );
        Ok(index)
    }

    pub(super) fn is_bool_type(&self, ctx: &Context, ty: TypeHandle) -> bool {
        ty.deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|int| int.width() == 1)
    }

    /// An expression used as a number (`undefined` becomes `NaN`).
    pub(super) fn number_operand(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.arithmetic(ctx, value);
        ensure!(
            kind(ctx, value)? == Kind::Number,
            "a number is required here"
        );
        Ok(value)
    }

    /// An optional numeric argument, or `default`.
    pub(super) fn number_or(
        &mut self,
        ctx: &mut Context,
        expr: Option<&'s Expr>,
        default: f64,
    ) -> Result<Value> {
        match expr {
            Some(Expr::Undefined) | None => Ok(self.number(ctx, default)),
            Some(expr) => self.number_operand(ctx, expr),
        }
    }

    pub(super) fn typed_operand(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
        want: Kind,
        what: &str,
    ) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.defined(ctx, value)?;
        ensure!(kind(ctx, value)? == want, "{what} is required here");
        Ok(value)
    }

    /// ECMAScript `ToString` of a primitive, as `console.log` and `+` display it.
    pub(super) fn display(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if let Some(inner) = optional_value(ctx, value.get_type(ctx)) {
            if inner.deref(ctx).is::<StringType>() {
                return Ok(self.emit_value(ctx, |ctx| ToStringOp::new(ctx, value)));
            }
            let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            return self.choose_string(
                ctx,
                undefined,
                |b, ctx| Ok(b.string(ctx, "undefined")),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                    b.choose_string(
                        ctx,
                        null,
                        |b, ctx| Ok(b.string(ctx, "null")),
                        |b, ctx| {
                            let op = UnwrapOp::new(ctx, value)?;
                            let defined = b.append_value(ctx, &op);
                            b.display(ctx, defined)
                        },
                    )
                },
            );
        }
        Ok(match kind(ctx, value)? {
            Kind::NumberString => {
                let tag = self.emit_value(ctx, |ctx| {
                    perry_mir::variants::VariantTagOp::new(ctx, value)
                });
                let number = self.string(ctx, "number");
                let is_number = self.compare(ctx, CompareKind::Eq, tag, number);
                self.choose_string(
                    ctx,
                    is_number,
                    |b, ctx| {
                        let ty = FP64Type::get(ctx).into();
                        let payload = b.emit_value(ctx, |ctx| {
                            perry_mir::variants::VariantPayloadOp::new(ctx, value, ty)
                        });
                        b.display(ctx, payload)
                    },
                    |b, ctx| {
                        let ty = StringType::get(ctx).into();
                        Ok(b.emit_value(ctx, |ctx| {
                            perry_mir::variants::VariantPayloadOp::new(ctx, value, ty)
                        }))
                    },
                )?
            }
            Kind::BigInt => {
                let value = if value
                    .get_type(ctx)
                    .deref(ctx)
                    .downcast_ref::<IntegerType>()
                    .is_some_and(|int| int.width() == 64)
                {
                    let bigint = perry_mir::types::BigIntType::get(ctx).into();
                    self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, bigint))
                } else {
                    value
                };
                self.intrinsic(ctx, Intrinsic::BigIntToString, vec![value])
            }
            Kind::String => value,
            Kind::Json => self.intrinsic(ctx, Intrinsic::JsonDisplay, vec![value]),
            Kind::Number => self.intrinsic(ctx, Intrinsic::NumberToString, vec![value]),
            Kind::Boolean => self.choose_string(
                ctx,
                value,
                |b, ctx| Ok(b.string(ctx, "true")),
                |b, ctx| Ok(b.string(ctx, "false")),
            )?,
            _ => bail!("only strings, numbers and booleans can be converted to strings yet"),
        })
    }

    /// `typeof value`, including the active case of a closed primitive union.
    pub(super) fn type_of(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if optional_value(ctx, value.get_type(ctx)).is_some() {
            let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
            return self.choose_string(
                ctx,
                undefined,
                |b, ctx| Ok(b.string(ctx, "undefined")),
                |b, ctx| {
                    let null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                    b.choose_string(
                        ctx,
                        null,
                        |b, ctx| Ok(b.string(ctx, "object")),
                        |b, ctx| {
                            let defined = b.defined(ctx, value)?;
                            b.type_of(ctx, defined)
                        },
                    )
                },
            );
        }
        let name = match kind(ctx, value)? {
            Kind::NumberString => {
                return Ok(self.emit_value(ctx, |ctx| {
                    perry_mir::variants::VariantTagOp::new(ctx, value)
                }));
            }
            Kind::BigInt => "bigint",
            Kind::Number => "number",
            Kind::String => "string",
            Kind::Boolean => "boolean",
            Kind::Json => {
                return Ok(self.intrinsic(ctx, Intrinsic::JsonTypeOf, vec![value]));
            }
            _ => "object",
        };
        Ok(self.string(ctx, name))
    }
}

/// The settled type of `Promise<void>`: `undefined` as `number | undefined`.
pub(super) fn void_type(ctx: &mut Context) -> TypeHandle {
    let number = FP64Type::get(ctx).into();
    perry_mir::types::OptionalType::get(ctx, number).into()
}
