//! Value kinds and the ECMAScript conversions between typed MIR values.
use super::*;

#[derive(Clone, Copy)]
pub(super) enum DisplayMode {
    String,
    Console,
}

pub(super) fn is_integer64(ctx: &Context, ty: TypeHandle) -> bool {
    ty.deref(ctx)
        .downcast_ref::<IntegerType>()
        .is_some_and(|integer| integer.width() == 64)
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
    let inner = present_type(ctx, ty);
    perry_mir::OptionalType::get_with(ctx, inner, states).into()
}

pub(super) fn expression_hint(ctx: &mut Context, expr: &Expr, base: TypeHandle) -> TypeHandle {
    match expr {
        Expr::Null => admitting(ctx, base, perry_mir::Absence::Null),
        Expr::Undefined => admitting(ctx, base, perry_mir::Absence::Undefined),
        _ => base,
    }
}

pub(super) fn merged_type(
    ctx: &Context,
    base: TypeHandle,
    left: TypeHandle,
    right: TypeHandle,
) -> TypeHandle {
    merge_absence(absence(ctx, left), absence(ctx, right)).map_or(base, |states| {
        perry_mir::OptionalType::get_with(ctx, present_type(ctx, base), states).into()
    })
}

pub(super) use perry_frontend::methods::Kind;

pub(super) trait KindExt {
    fn of(ctx: &Context, ty: TypeHandle) -> Option<Self>
    where
        Self: Sized;
}

impl KindExt for Kind {
    fn of(ctx: &Context, ty: TypeHandle) -> Option<Self> {
        let ty = ty.deref(ctx);
        if ty.is::<FP64Type>() {
            return Some(Self::Number);
        }
        if let Some(integer) = ty.downcast_ref::<IntegerType>() {
            return match integer.width() {
                1 => Some(Self::Boolean),
                64 => Some(Self::BigInt),
                _ => None,
            };
        }
        macro_rules! reference_kinds {
            ($($kind:ident => $mir:ident),+ $(,)?) => { $(
                if ty.is::<perry_mir::$mir>() { return Some(Self::$kind); }
            )+ };
        }
        reference_kinds! {
            String => StringType,
            BigInt => BigIntType,
            Bytes => BytesType,
            Url => UrlType,
            SearchParams => SearchParamsType,
            Json => JsonType,
            Date => DateType,
            Stats => StatsType,
            Headers => HeadersType,
            Response => ResponseType,
            Request => RequestType,
            Writable => WritableType,
            Writer => WriterType,
            Readable => ReadableType,
            ByteBuffer => ByteBufferType,
            ByobReader => ByobReaderType,
            Reader => ReaderType,
            ReadResult => ReadResultType,
            Instant => InstantType,
            PlainDate => PlainDateType,
            PlainDateTime => PlainDateTimeType,
            Abort => AbortType,
            Incoming => IncomingType,
            Exception => ExceptionType,
            Encoder => EncoderType,
            Decoder => DecoderType,
            Regex => RegexType,
            NumberString => NumberStringType,
            Array => ArrayType,
            Record => RecordType,
            Variant => VariantType,
            Resource => ResourceType,
            Settled => SettledType,
        }
        None
    }
}

pub(super) fn kind(ctx: &Context, value: Value) -> Result<Kind> {
    Kind::of(ctx, value.get_type(ctx)).context("unsupported value type in expression")
}

impl<'s> FunctionBuilder<'s> {
    /// Exact integer fields and assignments expose their values as source BigInts.
    pub(super) fn bigint_view(&mut self, ctx: &mut Context, value: Value) -> Value {
        if is_integer64(ctx, value.get_type(ctx)) {
            let bigint = BigIntType::get(ctx).into();
            self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, bigint))
        } else {
            value
        }
    }

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

    /// Extract the defined value of `T | undefined`, throwing `TypeError` on `undefined`.
    pub(super) fn defined(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if !self.is_optional(ctx, value) {
            return Ok(value);
        }
        let op = UnwrapOp::new(ctx, value)?;
        Ok(self.append_value(ctx, &op))
    }

    /// Dispatch an optional value, unwrapping only on its present branch.
    pub(super) fn match_optional(
        &mut self,
        ctx: &mut Context,
        value: Value,
        target: TypeHandle,
        undefined: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        null: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        present: impl FnOnce(&mut Self, &mut Context, Value) -> Result<Value>,
    ) -> Result<Value> {
        let is_undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
        self.choose(ctx, target, is_undefined, undefined, |b, ctx| {
            let is_null = b.emit_value(ctx, |ctx| IsNullOp::new(ctx, value));
            b.choose(ctx, target, is_null, null, |b, ctx| {
                let defined = b.defined(ctx, value)?;
                present(b, ctx, defined)
            })
        })
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
        let wanted = present_type(ctx, target);
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
        if ty.deref(ctx).is::<JsonType>()
            && ty != target
            && perry_mir::JsonSchema::supports_descriptor(ctx, target)
        {
            return self.decode_json(ctx, value, target);
        }
        let thrown = present_type(ctx, ty);
        let wanted = present_type(ctx, target);
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
        if (thrown.deref(ctx).is::<BigIntType>() && is_integer64(ctx, wanted))
            || (is_integer64(ctx, thrown) && wanted.deref(ctx).is::<BigIntType>())
        {
            let defined = self.defined(ctx, value)?;
            let casted = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, defined, wanted));
            return self.fit(ctx, casted, target);
        }
        if ty == target {
            Ok(value)
        } else if wanted.deref(ctx).is::<JsonType>() && !thrown.deref(ctx).is::<JsonType>() {
            let boxed = self.emit_value(ctx, |ctx| perry_mir::JsonBoxOp::new(ctx, value));
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
                return Ok(if admits_undefined(ctx, target) {
                    self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, target))
                } else {
                    self.emit_value(ctx, |ctx| UndefOp::new(ctx, target))
                });
            }
            let source = present_type(ctx, ty);
            let wanted = present_type(ctx, target);
            if (ty == source || absence(ctx, ty) == Some(perry_mir::Absence::Undefined))
                && let Some(dst_rec) = record_projection(ctx, source, wanted)
                && (!dst_rec.deref(ctx).fields()?.is_empty()
                    || self.shapes.class_of_type(ctx, wanted).is_some())
            {
                return self.project_record(ctx, value, ty == source, dst_rec, target);
            }
            let actual = describe_type(ctx, ty);
            let expected = describe_type(ctx, target);
            let shape_error = if let (Ok(src_rec), Ok(dst_rec)) = (
                TypedHandle::<RecordType>::from_handle(source, ctx),
                TypedHandle::<RecordType>::from_handle(wanted, ctx),
            ) && src_rec.deref(ctx).kind() == dst_rec.deref(ctx).kind()
                && matches!(
                    src_rec.deref(ctx).kind(),
                    RecordKind::Object | RecordKind::Tuple
                ) {
                ShapeError::IncompatibleRecordLayout { actual, expected }
            } else if absence(ctx, ty) != absence(ctx, target) {
                ShapeError::AbsenceMismatch { actual, expected }
            } else {
                ShapeError::TypeMismatch { actual, expected }
            };
            Err(LowerError::new(shape_error).into())
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

    /// An ECMAScript number used as an array index (out-of-bounds reads yield `undefined`).
    pub(super) fn index(&mut self, ctx: &mut Context, index: &'s Expr) -> Result<Value> {
        let index = self.expression(ctx, index)?;
        let index = self.arithmetic(ctx, index);
        ensure!(
            kind(ctx, index)? == Kind::Number,
            "array indices must be numbers"
        );
        Ok(index)
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
            let ty = StringType::get(ctx).into();
            return self.match_optional(
                ctx,
                value,
                ty,
                |b, ctx| Ok(b.string(ctx, "undefined")),
                |b, ctx| Ok(b.string(ctx, "null")),
                |b, ctx, defined| b.display_as(ctx, defined, mode),
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
                let value = self.bigint_view(ctx, value);
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
            let ty = StringType::get(ctx).into();
            return self.match_optional(
                ctx,
                value,
                ty,
                |b, ctx| Ok(b.string(ctx, "undefined")),
                |b, ctx| Ok(b.string(ctx, "object")),
                |b, ctx, defined| b.type_of(ctx, defined),
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

/// A named destination record whose fields all exist in the named source record.
fn record_projection(
    ctx: &Context,
    source: TypeHandle,
    wanted: TypeHandle,
) -> Option<TypedHandle<RecordType>> {
    let src_rec = TypedHandle::<RecordType>::from_handle(source, ctx).ok()?;
    let dst_rec = TypedHandle::<RecordType>::from_handle(wanted, ctx).ok()?;
    let (src, dst) = (src_rec.deref(ctx), dst_rec.deref(ctx));
    if src.is_tuple() || dst.is_tuple() {
        return None;
    }
    let fields = src.fields().ok()?;
    dst.fields()
        .ok()?
        .iter()
        .all(|(name, _)| fields.iter().any(|(source, _)| source == name))
        .then_some(dst_rec)
}

/// The settled type of `Promise<void>`: `undefined` as `number | undefined`.
pub(super) fn void_type(ctx: &mut Context) -> TypeHandle {
    let number = FP64Type::get(ctx).into();
    perry_mir::OptionalType::get(ctx, number).into()
}

fn describe_type(ctx: &Context, ty: TypeHandle) -> String {
    if let Some((k, v, _)) = crate::collections::map_entry_types(ctx, ty) {
        format!("Map<{}, {}>", describe_type(ctx, k), describe_type(ctx, v))
    } else if crate::collections::set_box_record(ctx, ty).is_some() {
        "Set".into()
    } else if let Some((k, v, _)) = crate::collections::dict_entry_types(ctx, ty) {
        format!(
            "Record<{}, {}>",
            describe_type(ctx, k),
            describe_type(ctx, v)
        )
    } else {
        ty.disp(ctx).to_string()
    }
}
