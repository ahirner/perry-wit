//! Value kinds and the ECMAScript conversions between typed MIR values.
use super::*;

#[derive(Clone, Copy)]
pub(super) enum DisplayMode {
    String,
    Console,
}

pub(super) fn merge_absence(
    left: Option<perry_mir::Absence>,
    right: Option<perry_mir::Absence>,
) -> Option<perry_mir::Absence> {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) if left == right => Some(left),
        (Some(_), Some(_)) => Some(perry_mir::Absence::Both),
    }
}

/// `ty` widened to also admit `state`: `T | undefined` from `T`, and `T | null | undefined`
/// from `T | null`.
pub(super) fn admitting(
    ctx: &mut Context,
    ty: TypeHandle,
    state: perry_mir::Absence,
) -> TypeHandle {
    let states = match absence(ctx, ty) {
        Some(existing) if existing != state => perry_mir::Absence::Both,
        _ => state,
    };
    let inner = optional_value(ctx, ty).unwrap_or(ty);
    perry_mir::OptionalType::get_with(ctx, inner, states).into()
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
    if ty.is::<perry_mir::BigIntType>() {
        Ok(Kind::BigInt)
    } else if ty.is::<FP64Type>() {
        Ok(Kind::Number)
    } else if ty.is::<StringType>() {
        Ok(Kind::String)
    } else if ty.is::<perry_mir::ResourceType>() {
        Ok(Kind::Resource)
    } else if ty.is::<perry_mir::NumberStringType>() {
        Ok(Kind::NumberString)
    } else if ty.is::<perry_mir::VariantType>() {
        Ok(Kind::Variant)
    } else if ty.is::<RecordType>() {
        Ok(Kind::Record)
    } else if ty.is::<ArrayType>() {
        Ok(Kind::Array)
    } else if ty.is::<ExceptionType>() {
        Ok(Kind::Exception)
    } else if ty.is::<BytesType>() {
        Ok(Kind::Bytes)
    } else if ty.is::<perry_mir::UrlType>() {
        Ok(Kind::Url)
    } else if ty.is::<perry_mir::SearchParamsType>() {
        Ok(Kind::SearchParams)
    } else if ty.is::<JsonType>() {
        Ok(Kind::Json)
    } else if ty.is::<perry_mir::RegexType>() {
        Ok(Kind::Regex)
    } else if ty.is::<perry_mir::DecoderType>() {
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
    } else if ty.is::<perry_mir::ByteBufferType>() {
        Ok(Kind::ByteBuffer)
    } else if ty.is::<perry_mir::ByobReaderType>() {
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

    /// View `value` with `undefined` excluded from its absence states (after a `!== undefined` check).
    pub(super) fn strip_undefined(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        let ty = value.get_type(ctx);
        match optional_parts(ctx, ty) {
            Some((_, perry_mir::Absence::Undefined)) => self.defined(ctx, value),
            Some((inner, perry_mir::Absence::Both)) => {
                let target =
                    perry_mir::OptionalType::get_with(ctx, inner, perry_mir::Absence::Null).into();
                let is_null = self.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
                self.choose(
                    ctx,
                    target,
                    is_null,
                    |b, ctx| Ok(b.emit_value(ctx, |ctx| NullOp::new(ctx, target))),
                    |b, ctx| {
                        let defined = b.defined(ctx, value)?;
                        b.fit(ctx, defined, target)
                    },
                )
            }
            _ => Ok(value),
        }
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
        if wanted.deref(ctx).is::<perry_mir::NumberStringType>()
            && (ty.deref(ctx).is::<FP64Type>() || ty.deref(ctx).is::<StringType>())
        {
            let tag = if ty.deref(ctx).is::<FP64Type>() {
                "number"
            } else {
                "string"
            };
            let boxed = self.emit_value(ctx, |ctx| {
                perry_mir::VariantNewOp::new(ctx, wanted, tag, Some(value))
            });
            return self.fit(ctx, boxed, target);
        }
        if target.deref(ctx).is::<perry_mir::ByteInputType>() {
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
                        |b, ctx| Ok(b.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, target))),
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
        if thrown.deref(ctx).is::<perry_mir::BigIntType>()
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
            && wanted.deref(ctx).is::<perry_mir::BigIntType>()
        {
            let defined = self.defined(ctx, value)?;
            let casted = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, defined, wanted));
            return self.fit(ctx, casted, target);
        }
        if ty == target {
            Ok(value)
        } else if wanted.deref(ctx).is::<JsonType>() && !thrown.deref(ctx).is::<JsonType>() {
            let json_ty: TypeHandle = JsonType::get(ctx).into();
            let boxed = if let Some(abs) = absence(ctx, ty)
                && abs.admits_undefined()
            {
                let is_undef = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
                self.choose(
                    ctx,
                    json_ty,
                    is_undef,
                    |b, ctx| Ok(b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, json_ty))),
                    |b, ctx| {
                        let def = b.defined(ctx, value)?;
                        let text =
                            b.emit_value(ctx, |ctx| perry_mir::JsonStringifyOp::new(ctx, def));
                        let text = b.defined(ctx, text)?;
                        Ok(b.intrinsic(ctx, Intrinsic::JsonParse, vec![text]))
                    },
                )?
            } else {
                let text = self.emit_value(ctx, |ctx| perry_mir::JsonStringifyOp::new(ctx, value));
                let text = self.defined(ctx, text)?;
                self.intrinsic(ctx, Intrinsic::JsonParse, vec![text])
            };
            self.fit(ctx, boxed, target)
        } else if optional_value(ctx, target) == Some(ty) {
            Ok(self.emit_value(ctx, |ctx| WrapOp::with_type(ctx, value, target)))
        } else if optional_value(ctx, target).is_some()
            && optional_value(ctx, target) == optional_value(ctx, ty)
        {
            Ok(self.emit_value(ctx, |ctx| OptionalCastOp::new(ctx, value, target)))
        } else if optional_value(ctx, ty) == Some(target) {
            self.defined(ctx, value)
        } else if perry_mir::can_widen(ctx, ty, target) {
            Ok(self.emit_value(ctx, |ctx| {
                perry_mir::AggregateViewOp::new(ctx, value, target)
            }))
        } else {
            // A value that never materializes takes any type.
            if self.never_values.contains(&value) {
                return Ok(
                    if absence(ctx, target).is_some_and(|a| a.admits_undefined()) {
                        self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, target))
                    } else {
                        self.emit_value(ctx, |ctx| UndefOp::new(ctx, target))
                    },
                );
            }
            let source = optional_value(ctx, ty).unwrap_or(ty);
            let wanted = optional_value(ctx, target).unwrap_or(target);
            if (ty == source || absence(ctx, ty) == Some(perry_mir::Absence::Undefined))
                && let Some((dst_rec, fields)) = record_projection(ctx, source, wanted)
                && (!fields.is_empty() || self.shapes.class_of_type(ctx, wanted).is_some())
            {
                return self.project_record(ctx, value, ty == source, dst_rec, fields, target);
            }
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

    /// Build a `dst_rec` from the matching fields of record `value` (already defined when
    /// `defined`); members outside the target's declaration carry over as JSON extras.
    fn project_record(
        &mut self,
        ctx: &mut Context,
        value: Value,
        defined: bool,
        dst_rec: TypedHandle<RecordType>,
        fields: Vec<ProjectedField>,
        target: TypeHandle,
    ) -> Result<Value> {
        let rec_val = if defined {
            value
        } else {
            self.defined(ctx, value)?
        };
        let mut projected = Vec::with_capacity(fields.len());
        for (name, src_ty, dst_ty) in fields {
            let field = if optional_value(ctx, src_ty).is_some()
                && absence(ctx, dst_ty).is_some_and(|a| a.admits_undefined())
            {
                let present = perry_mir::FieldPresentOp::new(ctx, rec_val, name.clone())?;
                let present = self.append_value(ctx, &present);
                self.choose(
                    ctx,
                    dst_ty,
                    present,
                    |b, ctx| {
                        let op = FieldGetOp::new(ctx, rec_val, name.clone())?;
                        let raw = b.field_read(ctx, &op)?;
                        b.fit(ctx, raw, dst_ty)
                    },
                    |b, ctx| {
                        Ok(b.emit_value(ctx, |ctx| perry_mir::OmittedOp::with_type(ctx, dst_ty)))
                    },
                )
            } else {
                let op = FieldGetOp::new(ctx, rec_val, name.clone())?;
                let raw = self.field_read(ctx, &op)?;
                self.fit(ctx, raw, dst_ty)
            };
            projected.push(field.with_context(|| {
                format!("record field {name} cannot become {}", dst_ty.disp(ctx))
            })?);
        }
        let projected = self.emit_value(ctx, |ctx| {
            let alloc = AllocOp::new(ctx, dst_rec, projected);
            alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(Vec::new()));
            alloc
        });
        self.intrinsic_void(ctx, Intrinsic::JsonSpread, vec![projected, rec_val]);
        self.fit(ctx, projected, target)
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
        self.display_as(ctx, value, DisplayMode::String)
    }

    pub(super) fn display_as(
        &mut self,
        ctx: &mut Context,
        value: Value,
        mode: DisplayMode,
    ) -> Result<Value> {
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
                            b.display_as(ctx, defined, mode)
                        },
                    )
                },
            );
        }
        Ok(match kind(ctx, value)? {
            Kind::NumberString => {
                let tag = self.emit_value(ctx, |ctx| perry_mir::VariantTagOp::new(ctx, value));
                let number = self.string(ctx, "number");
                let is_number = self.compare(ctx, CompareKind::Eq, tag, number);
                self.choose_string(
                    ctx,
                    is_number,
                    |b, ctx| {
                        let ty = FP64Type::get(ctx).into();
                        let payload = b.emit_value(ctx, |ctx| {
                            perry_mir::VariantPayloadOp::new(ctx, value, ty)
                        });
                        b.display_as(ctx, payload, mode)
                    },
                    |b, ctx| {
                        let ty = StringType::get(ctx).into();
                        Ok(b.emit_value(ctx, |ctx| {
                            perry_mir::VariantPayloadOp::new(ctx, value, ty)
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
                    let bigint = perry_mir::BigIntType::get(ctx).into();
                    self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, bigint))
                } else {
                    value
                };
                self.intrinsic(ctx, Intrinsic::BigIntToString, vec![value])
            }
            Kind::String => value,
            Kind::Json => self.intrinsic(ctx, Intrinsic::JsonDisplay, vec![value]),
            Kind::Number => self.intrinsic(
                ctx,
                match mode {
                    DisplayMode::String => Intrinsic::NumberToString,
                    DisplayMode::Console => Intrinsic::NumberInspect,
                },
                vec![value],
            ),
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
                return Ok(self.emit_value(ctx, |ctx| perry_mir::VariantTagOp::new(ctx, value)));
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
/// A target record field: its name, its type in the source record and in the target.
type ProjectedField = (String, TypeHandle, TypeHandle);

/// The target record of a projection from record `source` to record `wanted` and each of its
/// fields, when both are named records and the source has every target field.
fn record_projection(
    ctx: &Context,
    source: TypeHandle,
    wanted: TypeHandle,
) -> Option<(TypedHandle<RecordType>, Vec<ProjectedField>)> {
    let src_rec = TypedHandle::<RecordType>::from_handle(source, ctx).ok()?;
    let dst_rec = TypedHandle::<RecordType>::from_handle(wanted, ctx).ok()?;
    let (src, dst) = (src_rec.deref(ctx), dst_rec.deref(ctx));
    if src.is_tuple() || dst.is_tuple() {
        return None;
    }
    let src_fields = src.fields().ok()?;
    let fields = dst
        .fields()
        .ok()?
        .iter()
        .map(|(name, dst_ty)| {
            let (_, src_ty) = src_fields.iter().find(|(s, _)| s == name)?;
            Some((name.clone(), *src_ty, *dst_ty))
        })
        .collect::<Option<Vec<_>>>()?;
    Some((dst_rec, fields))
}

pub(super) fn void_type(ctx: &mut Context) -> TypeHandle {
    let number = FP64Type::get(ctx).into();
    perry_mir::OptionalType::get(ctx, number).into()
}
